//! Distribution policy belongs to the desktop shell, not the serial engine.

pub const MICROSOFT_STORE: bool = cfg!(feature = "microsoft-store");
pub const STORE_URI: &str = "ms-windows-store://pdp/?ProductId=9P5WS4PFD96H";

/// Distinct directory names prevent two installed editions sharing live files.
#[cfg(any(not(feature = "demo"), test))]
pub const fn application_directory(store: bool) -> &'static str {
    if store {
        "pigtail-store"
    } else {
        "pigtail"
    }
}
