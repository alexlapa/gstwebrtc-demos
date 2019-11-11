use failure::{Error, Fail};
use glib::object::ObjectExt;
use gstreamer::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use lazy_static::lazy_static;
use rand::Rng;
use serde_derive::{Deserialize, Serialize};
use serde_json;
use std::sync::{Arc, Mutex, Weak};
use tokio::{sync::mpsc};
use tokio_executor::Executor;

use tungstenite::Error as WsError;
use tungstenite::Message as WsMessage;

use crate::media::webrtcbin::*;
use futures::StreamExt;

lazy_static! {
    static ref RTP_CAPS_OPUS: Caps = {
        Caps::new_simple(
            "application/x-rtp",
            &[("media", &"audio"), ("encoding-name", &"OPUS"), ("payload", &(97_i32))],
        )
    };
    static ref RTP_CAPS_VP8: Caps = {
        Caps::new_simple(
            "application/x-rtp",
            &[("media", &"video"), ("encoding-name", &"VP8"), ("payload", &(96_i32))],
        )
    };
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JsonMsg {
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    Sdp {
        #[serde(rename = "type")]
        type_: String,
        sdp: String,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum MediaType {
    Audio,
    Video,
}

// Strong reference to our application state
#[derive(Debug, Clone)]
pub struct App(pub Arc<AppInner>);

// Weak reference to our application state
#[derive(Debug, Clone)]
struct AppWeak(Weak<AppInner>);

// Actual application state
#[allow(clippy::module_name_repetitions)]
#[derive(Debug)]
pub struct AppInner {
    // None if we wait for a peer to appear
    pub peer_id: Option<String>,
    pub webrtcbin: WebRtcBin,
    pub send_msg_tx: Mutex<mpsc::UnboundedSender<WsMessage>>,
    pub rtx: bool,
}

// Various error types for the different errors that can happen here
#[derive(Debug, Fail)]
#[fail(display = "WebSocket error: {:?}", _0)]
pub struct WebSocketError(WsError);

#[derive(Debug, Fail)]
#[fail(display = "GStreamer error: {:?}", _0)]
struct GStreamerError(String);

#[derive(Debug, Fail)]
#[fail(display = "Peer error: {:?}", _0)]
struct PeerError(String);

#[derive(Debug, Fail)]
#[fail(display = "Missing elements {:?}", _0)]
struct MissingElements(Vec<&'static str>);

// upgrade weak reference or return
#[macro_export]
macro_rules! upgrade_weak {
    ($x:ident, $r:expr) => {{
        match $x.upgrade() {
            Some(o) => o,
            None => return $r,
        }
    }};
    ($x:ident) => {
        upgrade_weak!($x, ())
    };
}

impl AppWeak {
    // Try upgrading a weak reference to a strong one
    fn upgrade(&self) -> Option<App> {
        self.0.upgrade().map(App)
    }
}

impl App {
    // Downgrade the strong reference to a weak reference
    fn downgrade(&self) -> AppWeak {
        AppWeak(Arc::downgrade(&self.0))
    }

    // Post an error message on the bus to asynchronously handle anything that
    // goes wrong on GStreamer threads
    fn post_error(&self, msg: &str) {
        gst_element_error!(self.0.webrtcbin.pipeline, LibraryError::Failed, (msg));
    }

    // Send a plain text message asynchronously over the WebSocket connection.
    // This can be called from any thread at any time and would send the
    // actual message from the IO threads of the runtime
    fn send_text_msg(&self, msg: String) -> Result<(), Error> {
        self.0
            .send_msg_tx
            .lock()
            .unwrap()
            .try_send(WsMessage::Text(msg))
            .map_err(|_| {
                WebSocketError(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Connection Closed",
                )))
                .into()
            })
    }

    // Once webrtcbin has create the offer SDP for us, handle it by sending it
    // to the peer via the WebSocket connection
    fn on_offer_created(&self, promise: &Promise) -> Result<(), Error> {
        let reply = match promise.wait() {
            PromiseResult::Replied => promise.get_reply().unwrap(),
            err => {
                return Err(GStreamerError(format!("Offer creation future got no reponse: {:?}", err)).into());
            }
        };

        let offer = reply
            .get_value("offer")
            .unwrap()
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .expect("Invalid argument");
        self.0
            .webrtcbin
            .webrtcbin
            .emit("set-local-description", &[&offer, &None::<Promise>])
            .unwrap();

        self.send_sdp_offer(offer.get_sdp().as_text().unwrap())
    }

    // Handle a newly decoded stream from decodebin, i.e. one of the streams
    // that the peer is sending to us. Connect it to newly create sink
    // elements and converters.
    #[allow(clippy::similar_names)]
    fn handle_media_stream(&self, pad: &Pad, media_type: MediaType) -> Result<(), Error> {
        println!("Trying to handle stream {:?}", media_type);

        let (q, conv, sink) = match media_type {
            MediaType::Audio => {
                let q = ElementFactory::make("queue", None).unwrap();
                let conv = ElementFactory::make("audioconvert", None).unwrap();
                let sink = ElementFactory::make("autoaudiosink", None).unwrap();
                let resample = ElementFactory::make("audioresample", None).unwrap();

                self.0
                    .webrtcbin
                    .pipeline
                    .add_many(&[&q, &conv, &resample, &sink])
                    .unwrap();
                Element::link_many(&[&q, &conv, &resample, &sink])?;

                resample.sync_state_with_parent()?;

                (q, conv, sink)
            }
            MediaType::Video => {
                let q = ElementFactory::make("queue", None).unwrap();
                let conv = ElementFactory::make("videoconvert", None).unwrap();
                let sink = ElementFactory::make("autovideosink", None).unwrap();

                self.0.webrtcbin.pipeline.add_many(&[&q, &conv, &sink]).unwrap();
                Element::link_many(&[&q, &conv, &sink])?;

                (q, conv, sink)
            }
        };

        q.sync_state_with_parent()?;
        conv.sync_state_with_parent()?;
        sink.sync_state_with_parent()?;

        let qpad = q.get_static_pad("sink").unwrap();
        pad.link(&qpad)?;

        Ok(())
    }

    // Handle a newly decoded decodebin stream and depending on its type, create
    // the relevant elements or simply ignore it
    fn on_incoming_decodebin_stream(&self, pad: &Pad) -> Result<(), Error> {
        let caps = pad.get_current_caps().unwrap();
        let name = caps.get_structure(0).unwrap().get_name();

        if name.starts_with("video/") {
            self.handle_media_stream(&pad, MediaType::Video)
        } else if name.starts_with("audio/") {
            self.handle_media_stream(&pad, MediaType::Audio)
        } else {
            println!("Unknown pad {:?}, ignoring", pad);
            Ok(())
        }
    }

    // Whenever there's a new incoming, encoded stream from the peer create a
    // new decodebin
    fn on_incoming_stream(&self, pad: &Pad) -> Result<(), Error> {
        // Early return for the source pads we're adding ourselves
        if pad.get_direction() != PadDirection::Src {
            return Ok(());
        }

        let decodebin = ElementFactory::make("decodebin", None).unwrap();
        let app_clone = self.downgrade();
        decodebin.connect_pad_added(move |_decodebin, pad| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_incoming_decodebin_stream(pad) {
                app.post_error(format!("Failed to handle decoded stream: {:?}", err).as_str());
            }
        });

        self.0.webrtcbin.pipeline.add(&decodebin).unwrap();

        decodebin.sync_state_with_parent()?;

        let sinkpad = decodebin.get_static_pad("sink").unwrap();
        pad.link(&sinkpad).unwrap();

        Ok(())
    }

    // Send our SDP offer via the WebSocket connection to the peer as JSON
    // message
    fn send_sdp_offer(&self, offer: String) -> Result<(), Error> {
        let message = serde_json::to_string(&JsonMsg::Sdp {
            type_: "offer".to_string(),
            sdp: offer,
        }).unwrap();

        println!("Sending SDP offer to peer: {}", message);

        self.send_text_msg(message)
    }

    // Send our SDP answer via the WebSocket connection to the peer as JSON
    // message
    fn send_sdp_answer(&self, answer: String) -> Result<(), Error> {
        let message = serde_json::to_string(&JsonMsg::Sdp {
            type_: "answer".to_string(),
            sdp: answer,
        }).unwrap();

        println!("Sending SDP answer to peer: {}", message);

        self.send_text_msg(message)
    }

    // Asynchronously send ICE candidates to the peer via the WebSocket
    // connection as a JSON message
    fn send_ice_candidate_message(&self, mlineindex: u32, candidate: String) -> Result<(), Error> {
        let message = serde_json::to_string(&JsonMsg::Ice {
            candidate,
            sdp_mline_index: mlineindex,
        })
        .unwrap();

        self.send_text_msg(message)
    }

    // Create a video test source plus encoder for the video stream we send to
    // the peer
    fn add_video_source(&self) -> Result<(), Error> {
        let videotestsrc = ElementFactory::make("videotestsrc", None).unwrap();
        let videoconvert = ElementFactory::make("videoconvert", None).unwrap();
        let queue = ElementFactory::make("queue", None).unwrap();
        let vp8enc = ElementFactory::make("vp8enc", None).unwrap();

        videotestsrc.set_property_from_str("pattern", "ball");
        videotestsrc.set_property("is-live", &true).unwrap();
        vp8enc.set_property("deadline", &1_i64).unwrap();

        let rtpvp8pay = ElementFactory::make("rtpvp8pay", None).unwrap();
        let queue2 = ElementFactory::make("queue", None).unwrap();

        self.0
            .webrtcbin
            .pipeline
            .add_many(&[&videotestsrc, &videoconvert, &queue, &vp8enc, &rtpvp8pay, &queue2])
            .unwrap();

        Element::link_many(&[&videotestsrc, &videoconvert, &queue, &vp8enc, &rtpvp8pay, &queue2])?;

        queue2.link_filtered(self.0.webrtcbin.as_ref(), Some(&*RTP_CAPS_VP8))?;

        Ok(())
    }

    // Create a audio test source plus encoders for the audio stream we send to
    // the peer
    fn add_audio_source(&self) -> Result<(), Error> {
        let audiotestsrc = ElementFactory::make("audiotestsrc", None).unwrap();
        let queue = ElementFactory::make("queue", None).unwrap();
        let audioconvert = ElementFactory::make("audioconvert", None).unwrap();
        let audioresample = ElementFactory::make("audioresample", None).unwrap();
        let queue2 = ElementFactory::make("queue", None).unwrap();
        let opusenc = ElementFactory::make("opusenc", None).unwrap();
        let rtpopuspay = ElementFactory::make("rtpopuspay", None).unwrap();
        let queue3 = ElementFactory::make("queue", None).unwrap();

        audiotestsrc.set_property_from_str("wave", "red-noise");
        audiotestsrc.set_property("is-live", &true).unwrap();

        self.0
            .webrtcbin
            .pipeline
            .add_many(&[
                &audiotestsrc,
                &queue,
                &audioconvert,
                &audioresample,
                &queue2,
                &opusenc,
                &rtpopuspay,
                &queue3,
            ])
            .unwrap();

        Element::link_many(&[
            &audiotestsrc,
            &queue,
            &audioconvert,
            &audioresample,
            &queue2,
            &opusenc,
            &rtpopuspay,
            &queue3,
        ])?;

        queue3.link_filtered(self.0.webrtcbin.as_ref(), Some(&*RTP_CAPS_OPUS))?;

        Ok(())
    }


//    // Finish creating our pipeline and actually start it once the connection
//    // with the peer is there
//    async fn setup_pipeline(&self) -> Result<(), Error> {
//        println!("Start pipeline");
//
//        // Whenever there is a new stream incoming from the peer, handle it
//        let app_clone = self.downgrade();
//        self.0.webrtcbin.webrtcbin.connect_pad_added(move |_webrtc, pad| {
//            let app = upgrade_weak!(app_clone);
//
//            if let Err(err) = app.on_incoming_stream(pad) {
//                app.post_error(format!("Failed to handle incoming stream: {:?}", err).as_str());
//            }
//        });
//
//        let app_clone = self.downgrade();
//
//
//        let handle_peer_events = async {
//            while let Some(item) =  self.0.webrtcbin.subscribe().next().await {
//                match item {
//                    WebRtcBinEvent::OnNegotiationNeeded=> {
//                        let app = app_clone.upgrade().unwrap();
//
//                        let offer = app.0.webrtcbin.create_offer().await.unwrap();
//
//                        app.0.webrtcbin.set_local_description(Sdp::Offer(offer.clone()));
//
//                        app.send_sdp_offer(offer);
//                    },
//                    WebRtcBinEvent::OnIceCandidate(cand) => {
//                        let app = app_clone.upgrade().unwrap();
//                        app.send_ice_candidate_message(cand.sdp_mline_index, cand.candidate.into());
//                    },
//                    WebRtcBinEvent::PadAdded(pad) => {
//
//                    }
//                }
//            };
//        };
//
//        let add_sources = async {
//
//            // Create our audio/video sources we send to the peer
//            self.add_video_source().unwrap();
//            self.add_audio_source().unwrap();
//
//            // Enable RTX only for video, Chrome etc al fail SDP negotiation
//            // otherwise
//            if self.0.rtx {
//                let transceiver = self
//                    .0
//                    .webrtcbin
//                    .get_transceiver(0)
//                    .unwrap();
//                transceiver.set_property("do-nack", &true).unwrap();
//            }
//        };
//
//        futures::join!(handle_peer_events, add_sources);
//
//        Ok(())
//    }

    fn on_negotiation_needed(&self) -> Result<(), Error> {

        println!("Starting negotiation");

        let app_clone = self.downgrade();
        let promise = Promise::new_with_change_func(move |promise| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_offer_created(promise) {
                app.post_error(format!("Failed to send SDP offer: {:?}", err).as_str());
            }
        });

        self.0
            .webrtcbin
            .webrtcbin
            .emit("create-offer", &[&None::<Structure>, &promise])
            .unwrap();

        Ok(())
    }


    // Finish creating our pipeline and actually start it once the connection
    // with the peer is there
    async fn setup_pipeline(&self) -> Result<(), Error> {
        println!("Start pipeline");

        // Whenever (re-)negotiation is needed, do so but this is only needed if
        // we send the initial offer
        if self.0.peer_id.is_some() {
            let app_clone = self.downgrade();
            self.0
                .webrtcbin
                .webrtcbin
                .connect("on-negotiation-needed", false, move |values| {
                    let _webrtc = values[0].get::<Element>().unwrap();

                    let app = upgrade_weak!(app_clone, None);

                    tokio::runtime::current_thread::Runtime::new().unwrap().block_on(async move {
//                        let promise = Promise::new_with_change_func(move |promise| {
//                            println!("qweeqweqweqw");
//                        });
//
//                        app.0
//                            .webrtcbin
//                            .webrtcbin
//                            .emit("create-offer", &[&None::<Structure>, &promise])
//                            .unwrap();

                        let offer = app.0.webrtcbin.create_offer().await.unwrap();



//                        let promise = Promise::new_with_change_func(move |promise| {
//                            println!("qweeqweqweqw");
//                        });
//
//                        app.0
//                            .webrtcbin
//                            .webrtcbin
//                            .emit("create-offer", &[&None::<Structure>, &promise])
//                            .unwrap();

//                        println!("111");
//                        let offer = app.0.webrtcbin.create_offer().await.unwrap();
//                        println!("222");



//                        app.0.webrtcbin.set_local_description(Sdp::Offer(offer.clone()));
//                        println!("333");
//                        app.send_sdp_offer(offer);
//                        println!("444");
                    });

                    None
                })
                .unwrap();
        }

        // Whenever there is a new ICE candidate, send it to the peer
        let app_clone = self.downgrade();
        self.0
            .webrtcbin
            .webrtcbin
            .connect("on-ice-candidate", false, move |values| {
                let _webrtc = values[0].get::<Element>().expect("Invalid argument");
                let mlineindex = values[1].get::<u32>().expect("Invalid argument");
                let candidate = values[2].get::<String>().expect("Invalid argument");

                let app = upgrade_weak!(app_clone, None);

                if let Err(err) = app.send_ice_candidate_message(mlineindex, candidate) {
                    app.post_error(format!("Failed to send ICE candidate: {:?}", err).as_str());
                }

                None
            })
            .unwrap();

        // Whenever there is a new stream incoming from the peer, handle it
        let app_clone = self.downgrade();
        self.0.webrtcbin.webrtcbin.connect_pad_added(move |_webrtc, pad| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_incoming_stream(pad) {
                app.post_error(format!("Failed to handle incoming stream: {:?}", err).as_str());
            }
        });

        // Create our audio/video sources we send to the peer
        self.add_video_source()?;
        self.add_audio_source()?;

        // Enable RTX only for video, Chrome etc al fail SDP negotiation
        // otherwise
        if self.0.rtx {
            let transceiver = self
                .0
                .webrtcbin
                .get_transceiver(0)
                .unwrap();
            transceiver.set_property("do-nack", &true).unwrap();
        }

        Ok(())
    }

    // Send ID of the peer we want to talk to via the WebSocket connection
    fn setup_call(&self, peer_id: &str) -> Result<WsMessage, Error> {
        println!("Setting up signalling server call with {}", peer_id);
        Ok(WsMessage::Text(format!("SESSION {}", peer_id)))
    }

    // Once we got the HELLO message from the WebSocket connection, start
    // setting up the call
    fn handle_hello(&self) -> Result<Option<WsMessage>, Error> {
        if let Some(ref peer_id) = self.0.peer_id {
            self.setup_call(peer_id).map(Some)
        } else {
            // Wait for a peer to appear
            Ok(None)
        }
    }

    // Once the session is set up correctly we start our pipeline
    async fn handle_session_ok(&self) -> Result<Option<WsMessage>, Error> {
        self.setup_pipeline().await?;

        // And finally asynchronously start our pipeline
        let app_clone = self.downgrade();
        self.0.webrtcbin.pipeline.call_async(move |pipeline| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = pipeline.set_state(State::Playing) {
                app.post_error(format!("Failed to set pipeline to Playing: {:?}", err).as_str());
            }
        });

        Ok(None)
    }

    // Handle errors from the peer send to us via the WebSocket connection
    fn handle_error(&self, msg: &str) -> Result<Option<WsMessage>, Error> {
        println!("Got error message! {}", msg);

        Err(PeerError(msg.into()).into())
    }

    // Handle incoming SDP answers from the peer
    async fn handle_sdp(&self, type_: &str, sdp: &str) -> Result<Option<WsMessage>, Error> {
        if type_ == "answer" {
            print!("Received answer:\n{}\n", sdp);

            self.0.webrtcbin.set_remote_description(Sdp::Answer(sdp.to_owned())).await;

            Ok(None)
        } else if type_ == "offer" {

            print!("Received offer:\n{}\n", sdp);

            // FIXME: We need to do negotiation here based on what the peer
            // offers us in the SDP and what we can produce. For
            // example all RTCP or RTP header extensions we don't
            // understand have to be removed, and similarly we have to negotiate
            // the codecs.

            self.setup_pipeline().await?;

            let sdp = Sdp::Offer(sdp.to_owned());

            self.0.webrtcbin.set_remote_description(sdp).await;

            let answer = self.0.webrtcbin.create_answer().await.unwrap();

            self.0.webrtcbin.set_local_description(Sdp::Answer(answer.clone())).await;

            self.send_sdp_answer(answer);

            Ok(None)
        } else {
            Err(PeerError(format!("Sdp type is not \"answer\" but \"{}\"", type_)).into())
        }
    }

    // Handle incoming ICE candidates from the peer by passing them to webrtcbin
    async fn handle_ice(&self, sdp_mline_index: u32, candidate: &str) -> Result<Option<WsMessage>, Error> {
        self.0
            .webrtcbin
            .add_ice_candidate(IceCandidate{ sdp_mline_index, candidate: candidate.into() })
            .unwrap();

        Ok(None)
    }

    // Handle messages we got from the peer via the WebSocket connection
    async fn on_message(&self, msg: &str) -> Result<Option<WsMessage>, Error> {
        match msg {
            "HELLO" => self.handle_hello(),
            "SESSION_OK" => self.handle_session_ok().await,
            x if x.starts_with("ERROR") => self.handle_error(msg),
            _ => {
                let json_msg: JsonMsg = serde_json::from_str(msg)?;

                match json_msg {
                    JsonMsg::Sdp { type_, sdp } => self.handle_sdp(&type_, &sdp).await,
                    JsonMsg::Ice {
                        sdp_mline_index,
                        candidate,
                    } => self.handle_ice(sdp_mline_index, &candidate).await,
                }
            }
        }
    }

    // Handle WebSocket messages, both our own as well as WebSocket protocol
    // messages
    pub async fn handle_websocket_message(&self, message: WsMessage) -> Result<Option<WsMessage>, Error> {
        match message {
            WsMessage::Close(_) => Ok(Some(WsMessage::Close(None))),
            WsMessage::Ping(data) => Ok(Some(WsMessage::Pong(data))),
            WsMessage::Text(msg) => self.on_message(&msg).await,
            WsMessage::Binary(_) | WsMessage::Pong(_) => Ok(None),
        }
    }

    // Handle GStreamer messages coming from the pipeline
    pub fn handle_pipeline_message(&self, message: &Message) -> Result<Option<WsMessage>, Error> {
        match message.view() {
            MessageView::Error(err) => Err(GStreamerError(format!(
                "Error from element {}: {} ({})",
                err.get_src()
                    .map_or_else(|| String::from("None"), |s| String::from(s.get_path_string())),
                err.get_error(),
                err.get_debug().unwrap_or_else(|| String::from("None")),
            ))
            .into()),
            MessageView::Warning(warning) => {
                println!("Warning: \"{}\"", warning.get_debug().unwrap());
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    // Entry-point to start everything
    pub fn register_with_server(&self) {
        let our_id = rand::thread_rng().gen_range(10, 10_000);
        println!("Registering id {} with server", our_id);
        self.send_text_msg(format!("HELLO {}", our_id)).unwrap();
    }
}

pub fn check_plugins() -> Result<(), Error> {
    let needed = [
        "opus",
        "vpx",
        "nice",
        "webrtc",
        "dtls",
        "srtp",
        "rtpmanager",
        "videotestsrc",
        "audiotestsrc",
        "pulseaudio",
    ];

    let registry = Registry::get();
    let missing = needed
        .iter()
        .filter(|n| registry.find_plugin(n).is_none())
        .cloned()
        .collect::<Vec<_>>();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(MissingElements(missing).into())
    }
}
