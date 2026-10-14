//! macOS window focus: activate the owning application via AppKit. Raising one specific
//! window of a multi-window app would need the Accessibility API (AXRaise); activating the
//! app brings its key window forward, which covers the dialog-clicking use case.
use crate::proto::*;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSNumber, NSString};

/// The focused window, without enumerating the desktop.
///
/// The application is found with `NSWorkspace`'s `activeApplication`, not `frontmostApplication`:
/// the latter is a KVO-observed property that only updates while an AppKit run loop is running,
/// and the daemon has none, so it would keep answering with whatever was in front when the
/// daemon started. `activeApplication` is resolved from the window server on each call. It is
/// deprecated, and xcap uses it for exactly this reason (xcap 0.9.8,
/// `src/macos/impl_window.rs`).
///
/// Only then is a window list walked, for that one pid — `Desktop::windows` is expensive here,
/// because xcap's window handle stores only an id, so every accessor (`title`, `app_name`,
/// `pid`, the rect) re-runs `CGWindowListCopyWindowInfo` over every on-screen window.
pub fn focused_window() -> Result<Option<Window>> {
    let key = NSString::from_str("NSApplicationProcessIdentifier");
    let workspace = NSWorkspace::sharedWorkspace();
    #[allow(deprecated)]
    let active = workspace.activeApplication();
    let pid = active
        .and_then(|dict| dict.valueForKey(&key))
        .and_then(|pid| pid.downcast::<NSNumber>().ok())
        .map(|pid| pid.intValue());
    match pid {
        Some(pid) if pid > 0 => super::capture::window_of_pid(pid as u32),
        _ => Ok(None),
    }
}

pub fn focus(w: &Window) -> Result<()> {
    let pid = w.pid as i32;
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .ok_or_else(|| RdcError::NotFound(format!("no running application with pid {pid}")))?;
    #[allow(deprecated)]
    let ok = app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    if ok { Ok(()) } else { Err(RdcError::Backend(format!("macOS refused to activate pid {pid}"))) }
}
