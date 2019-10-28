mod app;
mod media;

use clap;
use failure::Error;
use futures::{compat::*, FutureExt as _, TryFutureExt as _};
use gst::*;
use gstreamer as gst;
use native_tls::TlsConnector;
use std::sync::{Arc, Mutex};
use tokio::{prelude::*, sync::mpsc};
use websocket::{self, message::OwnedMessage};

const STUN_SERVER: &str = "stun://stun.l.google.com:19302";

fn main() -> Result<(), Error> {
    gst::init()?;

    app::check_plugins()?;

    let (server, peer_id, rtx) = parse_args();

    let mut runtime = tokio::runtime::Runtime::new()?;

    println!("Connecting to server {}", server);
    runtime.block_on(ws_loop(server, peer_id, rtx).boxed().compat())?;
    // And now shut down the runtime
    runtime.shutdown_now().wait().unwrap();

    Ok(())
}

#[allow(clippy::similar_names)]
async fn ws_loop(server: String, peer_id: Option<String>, rtx: bool) -> Result<(), Error> {
    let ws_client = websocket::client::ClientBuilder::new(&server)?;
    let (stream, _) = Compat01As03::new(ws_client.async_connect(Some(setup_tls_connector()?)))
        .await
        .map_err(|err| Error::from(app::WebSocketError(err)))?;

    println!("connected");

    // Create basic pipeline
    let pipeline = gst::Pipeline::new(Some("main"));
    let webrtcbin = gst::ElementFactory::make("webrtcbin", None).unwrap();
    pipeline.add(&webrtcbin).unwrap();

    webrtcbin.set_property_from_str("stun-server", STUN_SERVER);
    webrtcbin.set_property_from_str("bundle-policy", "max-bundle");

    let bus = pipeline.get_bus().unwrap();

    // Send our bus messages via a futures channel to be handled
    // asynchronously
    let (send_gst_msg_tx, send_gst_msg_rx) = mpsc::unbounded_channel::<gst::Message>();
    let send_gst_msg_tx = Mutex::new(send_gst_msg_tx);
    bus.set_sync_handler(move |_, msg| {
        let _ = send_gst_msg_tx.lock().unwrap().try_send(msg.clone());
        gst::BusSyncReply::Pass
    });

    // Create our application control logic
    let (send_ws_msg_tx, send_ws_msg_rx) = mpsc::unbounded_channel::<OwnedMessage>();
    let app = app::App(Arc::new(app::AppInner {
        peer_id,
        pipeline,
        webrtcbin,
        send_msg_tx: Mutex::new(send_ws_msg_tx),
        rtx,
    }));

    // Start registration process with the server. This will insert
    // a message into the send_ws_msg channel that
    // will then be sent later
    app.register_with_server();

    // Split the stream into the receive part (stream) and send part
    // (sink)
    let (sink, stream) = stream.split();

    // Pass the WebSocket messages to our application control logic
    // and convert them into potential messages to send out
    let app_clone = app.clone();
    let ws_messages = stream
        .map_err(|err| Error::from(app::WebSocketError(err)))
        .and_then(move |msg| app_clone.handle_websocket_message(msg))
        .filter_map(|msg| msg);

    // Pass the GStreamer messages to the application control logic
    // and convert them into potential messages to send out
    let gst_messages = send_gst_msg_rx
        .map_err(Error::from)
        .and_then(move |msg| app.handle_pipeline_message(&msg))
        .filter_map(|msg| msg);

    // Merge the two outgoing message streams
    let sync_outgoing_messages = gst_messages.select(ws_messages);

    // And here collect all the asynchronous outgoing messages that
    // come from other threads
    let async_outgoing_messages = send_ws_msg_rx.map_err(Error::from);

    // Merge both outgoing messages streams and send them out
    // directly

    Compat01As03::new(
        sink.sink_map_err(|err| Error::from(app::WebSocketError(err)))
            .send_all(sync_outgoing_messages.select(async_outgoing_messages))
            .map(|_| ()),
    )
    .await
}

fn setup_tls_connector() -> Result<TlsConnector, native_tls::Error> {
    TlsConnector::builder().danger_accept_invalid_certs(true).build()
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
