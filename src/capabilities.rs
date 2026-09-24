//! Capabilities come from the Rust host, never a URL flag or browser storage.
use serde::Serialize;

#[derive(Serialize)]
pub struct Capabilities {
    pub dev_mode: bool,
    pub browser_capture: bool,
    pub application_capture: bool,
}

impl Capabilities {
    pub fn for_host(desktop: bool) -> Self {
        Self {
            dev_mode: cfg!(debug_assertions),
            // Capture remains experimental until desktop playback is reliable.
            browser_capture: cfg!(debug_assertions),
            application_capture: desktop && cfg!(all(debug_assertions, target_os = "macos")),
        }
    }
}
