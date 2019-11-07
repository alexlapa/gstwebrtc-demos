pub mod webrtcbin;

pub fn enable_logs() {
    gstreamer::debug_set_colored(true);
    gstreamer::debug_set_active(true);
    gstreamer::debug_set_default_threshold(gstreamer::DebugLevel::Debug);
}
