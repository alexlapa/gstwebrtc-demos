mod app;
mod media;

use std::sync::{Arc, Mutex, Weak};

use failure::{Error, Fail};

use lazy_static::lazy_static;
use rand::Rng;

use serde_derive::{Deserialize, Serialize};

use tokio::prelude::*;
use tokio::sync::mpsc;

use tungstenite::Error as WsError;
use tungstenite::Message as WsMessage;

use gstreamer::gst_element_error;
use gstreamer::prelude::*;

const STUN_SERVER: &str = "stun://stun.l.google.com:19302";

fn main() -> Result<(), Error> {
    gstreamer::init()?;

    app::check_plugins()?;

    let (server, peer_id, rtx) = parse_args();

    let mut runtime = tokio::runtime::Runtime::new()?;

    println!("Connecting to server {}", server);
    runtime.block_on(ws_loop(server, peer_id, rtx))?;
    // And now shut down the runtime
    runtime.shutdown_now();

    Ok(())
}

#[allow(clippy::similar_names)]
async fn ws_loop(server: String, peer_id: Option<String>, rtx: bool) -> Result<(), Error> {
    let url = url::Url::parse(&server)?;
    let (ws, _) = tokio_tungstenite::connect_async(url).await?;
    let (ws_w, ws_r) = ws.split();
    println!("connected");

    let webrtcbin = media::webrtcbin::WebRtcBin::new("webrtcbin");

    webrtcbin.set_bundle_policy(media::webrtcbin::RtcBundlePolicy::MaxBundle);
    webrtcbin.set_stun_server(STUN_SERVER);

    let bus = webrtcbin.pipeline.get_bus().unwrap();

    // Send our bus messages via a futures channel to be handled asynchronously
    let (send_gst_msg_tx, send_gst_msg_rx) = mpsc::unbounded_channel::<gstreamer::Message>();
    let send_gst_msg_tx = Mutex::new(send_gst_msg_tx);
    bus.set_sync_handler(move |_, msg| {
        let _ = send_gst_msg_tx.lock().unwrap().try_send(msg.clone());
        gstreamer::BusSyncReply::Drop
    });

    // Create our application control logic
    let (send_ws_msg_tx, send_ws_msg_rx) = mpsc::unbounded_channel::<WsMessage>();
    let app = app::App(Arc::new(app::AppInner {
        peer_id,
        webrtcbin,
        send_msg_tx: Mutex::new(send_ws_msg_tx),
        rtx
    }));

    // Start registration process with the server. This will insert a
    // message into the send_ws_msg channel that will then be sent later
    app.register_with_server();

    // Fuse all the streams and make them mutable so we can select over them
    let mut ws_r = ws_r.fuse();
    let mut send_gst_msg_rx = send_gst_msg_rx.fuse();
    let mut send_ws_msg_rx = send_ws_msg_rx.fuse();

    let mut ws_w = ws_w;

    // And now handle all messages from our streams
    loop {
        let ws_msg = futures::select! {
            // Pass the WebSocket messages to our application control logic
            // and convert them into potential messages to send out
            ws_msg = ws_r.select_next_some() => {
                app.handle_websocket_message(ws_msg?)?
            },
            // Pass the GStreamer messages to the application control logic
            // and convert them into potential messages to send out
            gst_msg = send_gst_msg_rx.select_next_some() => {
                app.handle_pipeline_message(&gst_msg)?
            },
            // Handle WebSocket messages we created asynchronously
            // to send them out now
            ws_msg = send_ws_msg_rx.select_next_some() => Some(ws_msg),
            // Once we're done, break the loop and return
            complete => break,
        };

        // If there's a message to send out, do so now
        if let Some(ws_msg) = ws_msg {
            ws_w.send(ws_msg).await?;
        }
    }

    Ok(())
}

fn parse_args() -> (String, Option<String>, bool) {
    let matches = clap::App::new("Sendrecv rust")
        .arg(
            clap::Arg::with_name("peer-id")
                .help("String ID of the peer to connect to")
                .long("peer-id")
                .required(false)
                .takes_value(true),
        )
        .arg(
            clap::Arg::with_name("server")
                .help("Signalling server to connect to")
                .long("server")
                .required(false)
                .takes_value(true),
        )
        .arg(
            clap::Arg::with_name("rtx")
                .help("Enable retransmissions (RTX)")
                .long("rtx")
                .required(false),
        )
        .get_matches();

    let server = matches.value_of("server").unwrap_or("ws://localhost:8443");

    let peer_id = matches.value_of("peer-id");

    let rtx = matches.is_present("rtx");

    (server.to_string(), peer_id.map(String::from), rtx)
}
