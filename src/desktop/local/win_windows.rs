//! Windows-specific pieces: absolute pointer positioning across the whole virtual desktop and
//! window focus. enigo's absolute move normalises against the primary monitor only, so on a
//! multi-monitor desktop it cannot reach secondary displays; we send the input ourselves.

use crate::proto::*;
use std::ffi::c_void;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, SW_RESTORE, SendMessageTimeoutW, SetForegroundWindow, ShowWindow, WM_NULL,
};
use windows::core::PWSTR;

/// The foreground window via `GetForegroundWindow`: one handle, one title read, one process
/// name lookup. Orders of magnitude cheaper than enumerating every top-level window.
pub fn focused_window() -> Result<Option<Window>> {
    // SAFETY: plain Win32 queries on a handle we just obtained; every call tolerates a stale
    // or NULL handle by returning zero/error, which we map to None.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return Ok(None);
        }
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; (len.max(0) as usize) + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let app = process_image_name(pid).unwrap_or_default();
        let mut r = windows::Win32::Foundation::RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        Ok(Some(Window {
            id: hwnd.0 as usize as u64,
            pid,
            app,
            title,
            rect: Rect {
                x: r.left,
                y: r.top,
                w: (r.right - r.left).max(0) as u32,
                h: (r.bottom - r.top).max(0) as u32,
            },
            focused: true,
            minimized: IsIconic(hwnd).as_bool(),
        }))
    }
}

/// `foo.exe` → `foo`, matching what xcap reports as the app name.
fn process_image_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: the handle is closed before returning; the buffer length is passed alongside it.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if !ok {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        let file = full.rsplit(['\\', '/']).next().unwrap_or(&full);
        Some(file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string())
    }
}

/// Make every metric and capture in this process use physical pixels, so `GetSystemMetrics`
/// agrees with the physical geometry xcap reports. Call once at startup, before any UI call.
pub fn set_dpi_aware() {
    // SAFETY: plain Win32 call; failure (already set) is harmless.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// The virtual desktop rectangle in physical pixels (all monitors), as Windows reports it.
pub fn virtual_screen() -> Rect {
    // SAFETY: GetSystemMetrics has no preconditions.
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
        }
    }
}

/// Move the pointer to an absolute position in virtual-desktop pixels.
///
/// With `MOUSEEVENTF_VIRTUALDESK`, `dx`/`dy` are normalised so that 0 and 65535 map to the
/// edges of the whole virtual screen, not the primary monitor. See the `mouse_event` remarks in
/// the Win32 docs for the rounding used here.
pub fn move_absolute(x: i32, y: i32) -> Result<()> {
    let vs = virtual_screen();
    let norm = |v: i32, origin: i32, extent: u32| -> i32 {
        let span = (extent as i64 - 1).max(1);
        let rel = (v as i64 - origin as i64).clamp(0, span);
        ((rel * 65535 + span / 2) / span) as i32
    };
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: norm(x, vs.x, vs.w),
                dy: norm(y, vs.y, vs.h),
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: `input` is a fully initialised INPUT and the size matches.
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(RdcError::Backend(format!(
            "SendInput moved 0 of 1 events (blocked by UIPI or another session?): {}",
            windows::core::Error::from_thread()
        )))
    }
}

fn hwnd(id: u64) -> HWND {
    HWND(id as usize as *mut c_void)
}

/// Bring a window to the foreground.
///
/// Windows only lets the process that currently owns the foreground (or one it has handed the
/// right to) call `SetForegroundWindow` successfully. Attaching our thread's input queue to the
/// current foreground window's thread is the long-standing way to be allowed. We refuse to
/// attach to a thread that does not answer a message within 200 ms, because attaching to a hung
/// thread can hang us too.
pub fn focus(w: &Window) -> Result<()> {
    let target = hwnd(w.id);
    // SAFETY: plain Win32 calls on a window handle we got from enumeration; all of them tolerate
    // a stale handle by failing.
    unsafe {
        if IsIconic(target).as_bool() {
            let _ = ShowWindow(target, SW_RESTORE);
        }
        let fg = GetForegroundWindow();
        let mut fg_pid = 0u32;
        let fg_thread = if fg.0.is_null() { 0 } else { GetWindowThreadProcessId(fg, Some(&mut fg_pid)) };
        let me = GetCurrentThreadId();
        let fg_responsive = !fg.0.is_null()
            && SendMessageTimeoutW(fg, WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG | SMTO_BLOCK, 200, None).0 != 0;
        let attached =
            fg_responsive && fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, true).as_bool();
        let _ = BringWindowToTop(target);
        let mut ok = SetForegroundWindow(target).as_bool();
        if !ok && nudge_input() {
            // Windows grants foreground rights to the process that most recently sent input.
            // A zero-length mouse move counts and changes no key or pointer state.
            let _ = BringWindowToTop(target);
            ok = SetForegroundWindow(target).as_bool();
        }
        if attached {
            let _ = AttachThreadInput(me, fg_thread, false);
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
        if ok || GetForegroundWindow() == target {
            Ok(())
        } else {
            Err(RdcError::Backend(format!("Windows refused to bring {} ({}) to the foreground", w.app, w.title)))
        }
    }
}

/// Send a relative mouse move of zero pixels. Returns whether Windows accepted it.
fn nudge_input() -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 },
        },
    };
    // SAFETY: fully initialised INPUT, correct size.
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) == 1 }
}
