use anyhow::Result;
use futures::channel::oneshot;
use glib::{error::BoolError, object::ObjectExt};
use gstreamer::*;
use gstreamer_sdp::*;
use gstreamer_webrtc::*;
use thiserror::Error;

static ELEMENT_NAME: &str = "webrtcbin";

static PROPS_BUNDLE_POLICY: &str = "bundle-policy";
static PROPS_STUN_SERVER: &str = "stun-server";
static SIGS_SET_LOCAL_DESCRIPTION: &str = "set-local-description";
static SIGS_SET_REMOTE_DESCRIPTION: &str = "set-remote-description";
static SIGS_CREATE_OFFER: &str = "create-offer";
static SIGS_CREATE_ANSWER: &str = "create-offer";

#[derive(Error, Debug)]
pub enum WebRtcErr {
    #[error("Canceled")]
    Canceled(#[from] oneshot::Canceled),
    #[error("{0}")]
    Whatever(String),
}

pub enum RtcBundlePolicy {
    None = 0,
    Balanced = 1,
    MaxCompat = 2,
    MaxBundle = 3,
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
pub struct WebRtcBin {
    pipeline: Pipeline,
    webrtcbin: Element,
}

// pub enum WebRtcBinEvents {}

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

    pub fn set_bundle_policy(&self, bundle_policy: RtcBundlePolicy) -> Result<(), BoolError> {
        let bundle_policy = bundle_policy as i32;
        self.webrtcbin.set_property(PROPS_BUNDLE_POLICY, &bundle_policy)
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

    //    pub subscribe(&self) -> Stream<> {
    //
    //    }
}

impl AsRef<Element> for WebRtcBin {
    fn as_ref(&self) -> &Element {
        &self.webrtcbin
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer::*;

    #[test]
    fn set_stun_server() {
        gstreamer::init().unwrap();

        let webrtcbin = WebRtcBin::new("w/e");

        webrtcbin.set_stun_server("some_stun_server");

        let stun_server: String = webrtcbin
            .webrtcbin
            .get_property(PROPS_STUN_SERVER)
            .unwrap()
            .get()
            .unwrap();

        assert_eq!(stun_server, String::from("some_stun_server"));
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
}
