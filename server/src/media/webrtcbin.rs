use anyhow::Result;
use futures::{
    channel::{mpsc, oneshot},
    stream::LocalBoxStream,
};
use glib::{error::BoolError, object::ObjectExt};
use gstreamer::*;
use gstreamer_sdp::*;
use gstreamer_webrtc::*;
use thiserror::Error;
use std::borrow::Cow;

static ELEMENT_NAME: &str = "webrtcbin";

static PROPS_BUNDLE_POLICY: &str = "bundle-policy";
static PROPS_STUN_SERVER: &str = "stun-server";
static SIGS_SET_LOCAL_DESCRIPTION: &str = "set-local-description";
static SIGS_SET_REMOTE_DESCRIPTION: &str = "set-remote-description";
static SIGS_CREATE_OFFER: &str = "create-offer";
static SIGS_CREATE_ANSWER: &str = "create-answer";
static SIGS_ADD_ICE_CANDIDATE: &str = "add-ice-candidate";
static SIGS_ON_NEGOTIATION_NEEDED: &str = "on-negotiation-needed";
static SIGS_ON_ICE_CANDIDATE: &str = "on-ice-candidate";
static SIGS_GET_TRANSCEIVER: &str = "get-transceiver";

#[derive(Error, Debug)]
pub enum WebRtcErr {
    #[error("Canceled")]
    Canceled(#[from] oneshot::Canceled),
    #[error("{0}")]
    Whatever(String),
}

pub enum RtcBundlePolicy {
    Balanced,
    MaxCompat,
    MaxBundle,
}

impl AsRef<str> for RtcBundlePolicy {
    fn as_ref(&self) -> &str {
        match self {
            Self::Balanced => "balanced",
            Self::MaxCompat => "max-compat",
            Self::MaxBundle => "max-bundle",
        }
    }
}

pub enum Sdp {
    Answer(String),
    Offer(String),
}

impl Into<WebRTCSessionDescription> for Sdp {
    fn into(self) -> WebRTCSessionDescription {
        match self {
            Sdp::Answer(sdp) => {
                WebRTCSessionDescription::new(WebRTCSDPType::Answer, SDPMessage::parse_buffer(sdp.as_bytes()).unwrap())
            }
            Sdp::Offer(sdp) => {
                WebRTCSessionDescription::new(WebRTCSDPType::Offer, SDPMessage::parse_buffer(sdp.as_bytes()).unwrap())
            }
        }
    }
}

#[derive(Debug)]
pub struct IceCandidate<'a> {
    pub sdp_mline_index: u32,
    pub candidate: Cow<'a, str>,
}

#[derive(Debug)]
pub struct WebRtcBin {
    pub pipeline: Pipeline,
    pub webrtcbin: Element,
}

#[derive(Debug)]
pub enum WebRtcBinEvent<'a, 'b> {
    OnNegotiationNeeded,
    OnIceCandidate(IceCandidate<'a>),
    // TODO
    PadAdded(&'b Pad),
}

impl WebRtcBin {
    pub fn new(name_prefix: &str) -> Self {
        let pipeline = Pipeline::new(Some(&format!("{}_pipeline", name_prefix)));
        let webrtcbin = ElementFactory::make(ELEMENT_NAME, Some(&format!("{}_webrtcbin", name_prefix))).unwrap();

        pipeline.add(&webrtcbin).unwrap();
        pipeline.set_state(State::Ready).unwrap();

        Self { pipeline, webrtcbin }
    }

    pub fn set_stun_server(&self, stun_server: &str) {
        self.webrtcbin.set_property_from_str(PROPS_STUN_SERVER, stun_server);
    }

    pub fn set_bundle_policy(&self, bundle_policy: RtcBundlePolicy) {
        self.webrtcbin
            .set_property_from_str(PROPS_BUNDLE_POLICY, bundle_policy.as_ref())
    }

    pub async fn create_offer(&self) -> Result<String> {
        fn handle_create_offer(promise: &Promise) -> Result<String> {
            match promise.wait() {
                PromiseResult::Replied => {
                    let answer = promise
                        .get_reply()
                        .ok_or_else(|| WebRtcErr::Whatever(String::from("No reply in reply")))?;
                    let answer = answer
                        .get_value("offer")
                        .ok_or_else(|| WebRtcErr::Whatever(String::from("No `offer` field in reply")))?;
                    let answer = answer
                        .get::<WebRTCSessionDescription>()
                        .ok_or_else(|| WebRtcErr::Whatever(String::from("Not `WebRTCSessionDescription`")))?;
                    let answer = answer
                        .get_sdp()
                        .as_text()
                        .ok_or_else(|| WebRtcErr::Whatever(String::from("SDP is not text")))?;

                    Ok(answer)
                }
                err => Err(WebRtcErr::Whatever(String::from("Unresolved promise")))?,
            }
        }

        let (tx, rx) = oneshot::channel();

        let promise = Promise::new_with_change_func(move |promise: &Promise| {
            println!("AZAZAZAZAZZAAZ");
            tx.send(handle_create_offer(promise)).unwrap();
        });

        self.webrtcbin
            .emit(SIGS_CREATE_OFFER, &[&None::<Structure>, &promise])
            .unwrap();

        rx.await?
    }

    pub async fn create_answer(&self) -> Result<String> {
        fn handle_create_answer_response(promise: &Promise) -> Result<String> {
            match promise.wait() {
                PromiseResult::Replied => {
                    let answer = promise
                        .get_reply()
                        .ok_or_else(|| WebRtcErr::Whatever(String::from("No reply in reply")))?;
                    let answer = answer
                        .get_value("answer")
                        .ok_or_else(|| WebRtcErr::Whatever(String::from("No `answer` field in reply")))?;
                    let answer = answer
                        .get::<WebRTCSessionDescription>()
                        .ok_or_else(|| WebRtcErr::Whatever(String::from("Not `WebRTCSessionDescription`")))?;
                    let answer = answer
                        .get_sdp()
                        .as_text()
                        .ok_or_else(|| WebRtcErr::Whatever(String::from("SDP is not text")))?;

                    Ok(answer)
                }
                err => Err(WebRtcErr::Whatever(String::from("Unresolved promise")))?,
            }
        }

        let (tx, rx) = oneshot::channel();

        let promise = Promise::new_with_change_func(move |promise: &Promise| {
            tx.send(handle_create_answer_response(promise)).unwrap();
        });

        self.webrtcbin
            .emit(SIGS_CREATE_ANSWER, &[&None::<Structure>, &promise])
            .unwrap();

        rx.await?
    }

    pub async fn set_local_description(&self, sdp: Sdp) {
        let description: WebRTCSessionDescription = sdp.into();

        let (tx, rx) = oneshot::channel();
        let promise = Promise::new_with_change_func(move |promise| {
            tx.send(()).unwrap();
        });
        self.webrtcbin
            .emit(SIGS_SET_LOCAL_DESCRIPTION, &[&description, &promise])
            .unwrap();

        rx.await.unwrap()
    }

    pub fn add_ice_candidate(&self, ice_candidate: IceCandidate) -> Result<(), BoolError> {


        let candidate:&str = &ice_candidate.candidate;
        self.webrtcbin
            .emit(
                SIGS_ADD_ICE_CANDIDATE,
                &[&ice_candidate.sdp_mline_index, &candidate],
            )
            .map(|_| ())
    }

    pub async fn set_remote_description(&self, sdp: Sdp) {
        let description: WebRTCSessionDescription = sdp.into();

        let (tx, rx) = oneshot::channel();
        let promise = Promise::new_with_change_func(move |promise| {
            tx.send(()).unwrap();
        });
        self.webrtcbin
            .emit(SIGS_SET_REMOTE_DESCRIPTION, &[&description, &promise])
            .unwrap();

        rx.await.unwrap()
    }

    pub fn get_transceiver(&self, idx: i32) -> Option<WebRTCRTPTransceiver> {
        self.webrtcbin
            .emit(SIGS_GET_TRANSCEIVER, &[&idx])
            .unwrap()
            .unwrap()
            .get::<WebRTCRTPTransceiver>()
    }

    pub fn subscribe(&self) -> LocalBoxStream<'static, WebRtcBinEvent> {
        let (tx, rx) = mpsc::unbounded();

        let tx_clone = tx.clone();
        self.webrtcbin
            .connect(SIGS_ON_NEGOTIATION_NEEDED, false, move |values| {
                tx_clone.unbounded_send(WebRtcBinEvent::OnNegotiationNeeded);
                None
            })
            .unwrap();

        let tx_clone = tx.clone();
        self.webrtcbin
            .connect(SIGS_ON_ICE_CANDIDATE, false, move |values| {
                let sdp_mline_index = values[1].get::<u32>().unwrap();
                let candidate = values[2].get::<String>().unwrap();

                tx_clone.unbounded_send(WebRtcBinEvent::OnIceCandidate(IceCandidate {
                    sdp_mline_index,
                    candidate:candidate.into(),
                }));
                None
            })
            .unwrap();

//        self.webrtcbin.connect_pad_added(move |_, pad| {
//            tx_clone.unbounded_send(WebRtcBinEvent::PadAdded(pad));
//        });

        Box::pin(rx)
    }
}

impl AsRef<Element> for WebRtcBin {
    fn as_ref(&self) -> &Element {
        &self.webrtcbin
    }
}

impl Drop for WebRtcBin {
    fn drop(&mut self) {
        self.pipeline.set_state(State::Null).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::StreamExt;

    #[test]
    fn set_stun_server() {
        gstreamer::init().unwrap();

        let webrtcbin = WebRtcBin::new("set_stun_server");

        webrtcbin.set_stun_server("some_stun_server");

        let stun_server: String = webrtcbin
            .webrtcbin
            .get_property(PROPS_STUN_SERVER)
            .unwrap()
            .get()
            .unwrap();

        assert_eq!(stun_server, String::from("some_stun_server"));
    }

    // TODO: actually it should not panic
    #[test]
    #[should_panic]
    fn set_bundle_policy() {
        gstreamer::init().unwrap();
        let webrtcbin = WebRtcBin::new("set_bundle_policy");

        webrtcbin.set_bundle_policy(RtcBundlePolicy::MaxBundle);
        //        webrtcbin.webrtcbin.set_property_from_str("bundle-policy",
        // "max-bundle");

        let bundle_policy: gstreamer_webrtc_sys::GstWebRTCBundlePolicy = webrtcbin
            .webrtcbin
            .get_property("bundle-policy")
            .unwrap()
            .get()
            .unwrap();
    }

    #[tokio::test]
    async fn create_offer_set_local() {
        gstreamer::init().unwrap();

        let webrtcbin = WebRtcBin::new("create_offer_set_local");
        let offer = webrtcbin.create_offer().await.unwrap();
        webrtcbin.set_local_description(Sdp::Offer(offer)).await
    }

    #[tokio::test]
    #[should_panic]
    async fn create_answer_no_remote() {
        gstreamer::init().unwrap();

        let webrtcbin = WebRtcBin::new("create_answer_no_remote");
        let answer = webrtcbin.create_answer().await.unwrap();
    }

    #[tokio::test]
    async fn ice_candidates_emitted() {
        gstreamer::init().unwrap();

        let webrtcbin = WebRtcBin::new("ice_candidates_emitted");
        webrtcbin
            .set_remote_description(Sdp::Offer(CHROME_OFFER.to_owned()))
            .await;

        let answer = webrtcbin.create_answer().await.unwrap();

        let mut events = webrtcbin.subscribe();

        webrtcbin.set_local_description(Sdp::Answer(answer)).await;

        while let Some(event) = events.next().await {
            match event {
                WebRtcBinEvent::OnIceCandidate(IceCandidate) => {
                    break;
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn get_transceiver() {
        gstreamer::init().unwrap();

        let webrtcbin = WebRtcBin::new("ice_candidates_emitted");
        webrtcbin
            .set_remote_description(Sdp::Offer(CHROME_OFFER.to_owned()))
            .await;

        let answer = webrtcbin.create_answer().await.unwrap();

        let mut events = webrtcbin.subscribe();

        webrtcbin.set_local_description(Sdp::Answer(answer)).await;

        assert!(webrtcbin.get_transceiver(0).is_some());
        assert!(webrtcbin.get_transceiver(1).is_some());
        assert_eq!(webrtcbin.get_transceiver(2), None);
    }

    static CHROME_OFFER: &str = r#"v=0
o=- 4517647007027865565 2 IN IP4 127.0.0.1
s=-
t=0 0
a=group:BUNDLE 0 1
a=msid-semantic: WMS kKcUH9YmSk4LXq8xD91lrUXISLLzrfZfwjks
m=audio 9 UDP/TLS/RTP/SAVPF 111 103 104 9 0 8 110 112 113 126
c=IN IP4 0.0.0.0
a=rtcp:9 IN IP4 0.0.0.0
a=ice-ufrag:0xoh
a=ice-pwd:kE6XMRzr5z7tC9glup0ffbda
a=ice-options:trickle
a=fingerprint:sha-256 9C:1C:C6:9A:BA:D1:ED:1C:E9:EE:90:07:04:77:A9:86:AB:5B:97:AC:D9:CA:47:B3:99:A4:4B:97:4E:43:11:14
a=setup:actpass
a=mid:0
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level
a=extmap:2 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01
a=extmap:3 urn:ietf:params:rtp-hdrext:sdes:mid
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id
a=extmap:5 urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id
a=sendrecv
a=msid:kKcUH9YmSk4LXq8xD91lrUXISLLzrfZfwjks d1ee9fa6-d529-4580-8827-f0885cb8209c
a=rtcp-mux
a=rtpmap:111 opus/48000/2
a=rtcp-fb:111 transport-cc
a=fmtp:111 minptime=10;useinbandfec=1
a=rtpmap:103 ISAC/16000
a=rtpmap:104 ISAC/32000
a=rtpmap:9 G722/8000
a=rtpmap:0 PCMU/8000
a=rtpmap:8 PCMA/8000
a=rtpmap:110 telephone-event/48000
a=rtpmap:112 telephone-event/32000
a=rtpmap:113 telephone-event/16000
a=rtpmap:126 telephone-event/8000
a=ssrc:4221376672 cname:tu6U1WvpjT+KLTzi
a=ssrc:4221376672 msid:kKcUH9YmSk4LXq8xD91lrUXISLLzrfZfwjks d1ee9fa6-d529-4580-8827-f0885cb8209c
a=ssrc:4221376672 mslabel:kKcUH9YmSk4LXq8xD91lrUXISLLzrfZfwjks
a=ssrc:4221376672 label:d1ee9fa6-d529-4580-8827-f0885cb8209c
m=video 9 UDP/TLS/RTP/SAVPF 98 100 96 97 99 101 102 122 127 121 125 107 108 109 124 120 123
c=IN IP4 0.0.0.0
a=rtcp:9 IN IP4 0.0.0.0
a=ice-ufrag:0xoh
a=ice-pwd:kE6XMRzr5z7tC9glup0ffbda
a=ice-options:trickle
a=fingerprint:sha-256 9C:1C:C6:9A:BA:D1:ED:1C:E9:EE:90:07:04:77:A9:86:AB:5B:97:AC:D9:CA:47:B3:99:A4:4B:97:4E:43:11:14
a=setup:actpass
a=mid:1
a=extmap:14 urn:ietf:params:rtp-hdrext:toffset
a=extmap:13 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time
a=extmap:12 urn:3gpp:video-orientation
a=extmap:2 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01
a=extmap:11 http://www.webrtc.org/experiments/rtp-hdrext/playout-delay
a=extmap:6 http://www.webrtc.org/experiments/rtp-hdrext/video-content-type
a=extmap:7 http://www.webrtc.org/experiments/rtp-hdrext/video-timing
a=extmap:8 http://tools.ietf.org/html/draft-ietf-avtext-framemarking-07
a=extmap:9 http://www.webrtc.org/experiments/rtp-hdrext/color-space
a=extmap:3 urn:ietf:params:rtp-hdrext:sdes:mid
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id
a=extmap:5 urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id
a=sendrecv
a=msid:kKcUH9YmSk4LXq8xD91lrUXISLLzrfZfwjks 3ed65e10-9993-4554-9138-647baa61c158
a=rtcp-mux
a=rtcp-rsize
a=rtpmap:96 VP8/90000
a=rtcp-fb:96 goog-remb
a=rtcp-fb:96 transport-cc
a=rtcp-fb:96 ccm fir
a=rtcp-fb:96 nack
a=rtcp-fb:96 nack pli
a=rtpmap:97 rtx/90000
a=fmtp:97 apt=96
a=rtpmap:98 VP9/90000
a=rtcp-fb:98 goog-remb
a=rtcp-fb:98 transport-cc
a=rtcp-fb:98 ccm fir
a=rtcp-fb:98 nack
a=rtcp-fb:98 nack pli
a=fmtp:98 profile-id=0
a=rtpmap:99 rtx/90000
a=fmtp:99 apt=98
a=rtpmap:100 VP9/90000
a=rtcp-fb:100 goog-remb
a=rtcp-fb:100 transport-cc
a=rtcp-fb:100 ccm fir
a=rtcp-fb:100 nack
a=rtcp-fb:100 nack pli
a=fmtp:100 profile-id=2
a=rtpmap:101 rtx/90000
a=fmtp:101 apt=100
a=rtpmap:102 H264/90000
a=rtcp-fb:102 goog-remb
a=rtcp-fb:102 transport-cc
a=rtcp-fb:102 ccm fir
a=rtcp-fb:102 nack
a=rtcp-fb:102 nack pli
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f
a=rtpmap:122 rtx/90000
a=fmtp:122 apt=102
a=rtpmap:127 H264/90000
a=rtcp-fb:127 goog-remb
a=rtcp-fb:127 transport-cc
a=rtcp-fb:127 ccm fir
a=rtcp-fb:127 nack
a=rtcp-fb:127 nack pli
a=fmtp:127 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f
a=rtpmap:121 rtx/90000
a=fmtp:121 apt=127
a=rtpmap:125 H264/90000
a=rtcp-fb:125 goog-remb
a=rtcp-fb:125 transport-cc
a=rtcp-fb:125 ccm fir
a=rtcp-fb:125 nack
a=rtcp-fb:125 nack pli
a=fmtp:125 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f
a=rtpmap:107 rtx/90000
a=fmtp:107 apt=125
a=rtpmap:108 H264/90000
a=rtcp-fb:108 goog-remb
a=rtcp-fb:108 transport-cc
a=rtcp-fb:108 ccm fir
a=rtcp-fb:108 nack
a=rtcp-fb:108 nack pli
a=fmtp:108 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f
a=rtpmap:109 rtx/90000
a=fmtp:109 apt=108
a=rtpmap:124 red/90000
a=rtpmap:120 rtx/90000
a=fmtp:120 apt=124
a=rtpmap:123 ulpfec/90000
a=ssrc-group:FID 1659291194 791273220
a=ssrc:1659291194 cname:tu6U1WvpjT+KLTzi
a=ssrc:1659291194 msid:kKcUH9YmSk4LXq8xD91lrUXISLLzrfZfwjks 3ed65e10-9993-4554-9138-647baa61c158
a=ssrc:1659291194 mslabel:kKcUH9YmSk4LXq8xD91lrUXISLLzrfZfwjks
a=ssrc:1659291194 label:3ed65e10-9993-4554-9138-647baa61c158
a=ssrc:791273220 cname:tu6U1WvpjT+KLTzi
a=ssrc:791273220 msid:kKcUH9YmSk4LXq8xD91lrUXISLLzrfZfwjks 3ed65e10-9993-4554-9138-647baa61c158
a=ssrc:791273220 mslabel:kKcUH9YmSk4LXq8xD91lrUXISLLzrfZfwjks
a=ssrc:791273220 label:3ed65e10-9993-4554-9138-647baa61c158"#;
}
