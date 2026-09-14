//! macOS window focus: activate the owning application via AppKit. Raising one specific
//! window of a multi-window app would need the Accessibility API (AXRaise); activating the
//! app brings its key window forward, which covers the dialog-clicking use case.
use crate::proto::*;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};

/// The focused window, without enumerating the desktop.
///
/// `Desktop::windows` is expensive here: xcap's `ImplWindow` stores only a window id, so every
/// accessor (`title`, `app_name`, `pid`, the rect) re-runs `CGWindowListCopyWindowInfo` over
/// every on-screen window, and `is_focused` additionally asks `NSWorkspace` — which makes
/// listing N windows cost O(N) full desktop snapshots. The audit lookup runs before every
/// action, so instead: ask `NSWorkspace` once which application is frontmost, then read the
/// accessors for that application's first window only.
///
/// When the frontmost application has no window in the on-screen list (a menu-bar-only agent,
/// or a window the capture list excludes), the application itself is still the honest answer
/// for "what was in front", so it is returned with an empty title rather than nothing.
pub fn focused_window() -> Result<Option<Window>> {
    let Some(app) = NSWorkspace::sharedWorkspace().frontmostApplication() else {
        return Ok(None);
    };
    let pid = app.processIdentifier();
    if pid <= 0 {
        return Ok(None);
    }
    let pid = pid as u32;
    if let Some(w) = super::capture::window_of_pid(pid)? {
        return Ok(Some(w));
    }
    let name = app.localizedName().map(|s| s.to_string()).unwrap_or_default();
    if name.is_empty() {
        return Ok(None);
    }
    Ok(Some(Window {
        id: 0,
        pid,
        app: name,
        title: String::new(),
        rect: Rect { x: 0, y: 0, w: 0, h: 0 },
        focused: true,
        minimized: false,
    }))
}

pub fn focus(w: &Window) -> Result<()> {
    let pid = w.pid as i32;
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .ok_or_else(|| RdcError::NotFound(format!("no running application with pid {pid}")))?;
    #[allow(deprecated)]
    let ok = app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    if ok { Ok(()) } else { Err(RdcError::Backend(format!("macOS refused to activate pid {pid}"))) }
}
