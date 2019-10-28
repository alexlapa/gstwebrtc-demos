
use gstreamer::*;
use glib::object::ObjectExt;
use glib::translate::ToGlibPtr;

pub enum RtcBundlePolicy {
    None = 0,
    Balanced = 1,
    MaxCompat = 2,
    MaxBundle = 3
}


pub struct WebRtcBin {
    element: Element
}

impl WebRtcBin {
    fn new(name: &str) -> Self {
        Self {
            element: ElementFactory::make("webrtcbin", Some(name)).unwrap()
        }
    }

    fn set_stun_server(&self, stun_server:&str) -> &Self {

        self.element.set_property_from_str("stun-server", stun_server);

        &self
    }

    fn set_bundle_policy(&self, bundle_policy: &RtcBundlePolicy) -> &Self {
        self.element.set_property("bundle-policy".to_glib_none().0, "max-bundle");

        &self
    }
}

//none (0) – GST_WEBRTC_BUNDLE_POLICY_NONE
//balanced (1) – GST_WEBRTC_BUNDLE_POLICY_BALANCED
//max-compat (2) – GST_WEBRTC_BUNDLE_POLICY_MAX_COMPAT
//max-bundle (3) – GST_WEBRTC_BUNDLE_POLICY_MAX_BUNDLE

