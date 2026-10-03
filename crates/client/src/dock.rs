//! The Dock badge, which counts open Inbox entries.

/// Shows `label` on the app's Dock icon, or clears it with `None`. Must run
/// on the main thread, as GPUI's view updates do.
#[cfg(target_os = "macos")]
pub fn set_badge(label: Option<&str>) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    let label = label.map(NSString::from_str);
    // SAFETY: `sharedApplication` and `dockTile` take no arguments and
    // return autoreleased objects or nil, and `setBadgeLabel:` takes an
    // NSString or nil. GPUI calls this on the main thread, where AppKit
    // wants it.
    unsafe {
        let app: Option<Retained<AnyObject>> = msg_send![class!(NSApplication), sharedApplication];
        let Some(app) = app else { return };
        let tile: Option<Retained<AnyObject>> = msg_send![&*app, dockTile];
        let Some(tile) = tile else { return };
        let _: () = msg_send![&*tile, setBadgeLabel: label.as_deref()];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_badge(_: Option<&str>) {}
