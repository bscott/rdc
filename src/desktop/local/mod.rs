//! In-process backend: xcap for capture, enigo for input, arboard for clipboard,
//! plus compositor-specific window management where the generic path falls short.

mod capture;
mod input;
#[cfg(target_os = "linux")]
mod win_hypr;
#[cfg(target_os = "macos")]
mod win_mac;
#[cfg(target_os = "windows")]
mod win_windows;
#[cfg(target_os = "windows")]
pub use win_windows::set_dpi_aware;

use super::{Desktop, find_window};
use crate::proto::*;
use async_trait::async_trait;
use input::InputWorker;

pub struct LocalDesktop {
    input: InputWorker,
    #[cfg(target_os = "linux")]
    hypr: Option<win_hypr::Hyprland>,
}

impl LocalDesktop {
    pub fn new() -> Result<Self> {
        Ok(Self {
            input: InputWorker::start(),
            #[cfg(target_os = "linux")]
            hypr: win_hypr::Hyprland::detect(),
        })
    }

    /// Human-readable description of which platform paths are active (for `doctor`).
    pub fn describe(&self) -> Vec<String> {
        #[allow(unused_mut)]
        let mut v = vec![format!("capture: xcap ({})", capture::backend_name())];
        #[cfg(target_os = "linux")]
        v.push(match &self.hypr {
            Some(_) => "windows: hyprctl (Hyprland detected)".into(),
            None => "windows: xcap generic (no Hyprland)".into(),
        });
        #[cfg(target_os = "macos")]
        v.push("windows: xcap list + AppKit/AX focus".into());
        #[cfg(target_os = "windows")]
        v.push(format!(
            "windows: xcap list + SetForegroundWindow; virtual desktop {:?}",
            win_windows::virtual_screen()
        ));
        v.push("input: enigo".into());
        v
    }

    fn logical_displays(&self) -> Result<Vec<Display>> {
        #[cfg(target_os = "linux")]
        if let Some(h) = &self.hypr {
            return h.displays();
        }
        capture::displays()
    }
}

#[async_trait]
impl Desktop for LocalDesktop {
    async fn displays(&self) -> Result<Vec<Display>> {
        self.logical_displays()
    }

    async fn screenshot(&self, req: ScreenshotReq) -> Result<Screenshot> {
        let displays = self.logical_displays()?;
        tokio::task::spawn_blocking(move || capture::screenshot(&displays, &req))
            .await
            .map_err(|e| RdcError::Backend(format!("capture task failed: {e}")))?
    }

    async fn windows(&self) -> Result<Vec<Window>> {
        #[cfg(target_os = "linux")]
        if let Some(h) = &self.hypr {
            return h.windows();
        }
        tokio::task::spawn_blocking(capture::windows)
            .await
            .map_err(|e| RdcError::Backend(format!("window task failed: {e}")))?
    }

    async fn focused_window(&self) -> Result<Option<Window>> {
        // Every backend here talks to a window server or a subprocess, so none of it belongs on
        // an executor thread. How long it may take is bounded by the caller; see
        // `server::routes::FOCUS_LOOKUP_TIMEOUT`.
        #[cfg(target_os = "linux")]
        if let Some(h) = &self.hypr {
            let h = *h;
            return tokio::task::spawn_blocking(move || h.focused_window())
                .await
                .map_err(|e| RdcError::Backend(format!("focused-window task failed: {e}")))?;
        }
        #[cfg(target_os = "macos")]
        {
            return tokio::task::spawn_blocking(win_mac::focused_window)
                .await
                .map_err(|e| RdcError::Backend(format!("focused-window task failed: {e}")))?;
        }
        #[cfg(target_os = "windows")]
        {
            return tokio::task::spawn_blocking(win_windows::focused_window)
                .await
                .map_err(|e| RdcError::Backend(format!("focused-window task failed: {e}")))?;
        }
        // X11 and other Linux compositors have no single-window query of their own.
        #[allow(unreachable_code)]
        tokio::task::spawn_blocking(capture::focused_window)
            .await
            .map_err(|e| RdcError::Backend(format!("focused-window task failed: {e}")))?
    }

    async fn focus(&self, target: WindowTarget) -> Result<()> {
        let windows = self.windows().await?;
        let w = find_window(&windows, &target)
            .ok_or_else(|| RdcError::NotFound(format!("no window matches {target:?}")))?
            .clone();
        #[cfg(target_os = "linux")]
        {
            if let Some(h) = &self.hypr {
                return h.focus(&w);
            }
            Err(RdcError::Unsupported("window focus without Hyprland".into()))
        }
        #[cfg(target_os = "macos")]
        {
            win_mac::focus(&w)
        }
        #[cfg(target_os = "windows")]
        {
            // Attaching to another thread's input queue can block; keep it off the executor.
            tokio::task::spawn_blocking(move || win_windows::focus(&w))
                .await
                .map_err(|e| RdcError::Backend(format!("focus task failed: {e}")))?
        }
    }

    async fn input(&self, action: InputAction) -> Result<()> {
        let displays = self.logical_displays()?;
        self.input.input(action, displays).await
    }

    async fn clipboard_get(&self) -> Result<String> {
        self.input.clipboard_get().await
    }

    async fn clipboard_set(&self, text: String) -> Result<()> {
        self.input.clipboard_set(text).await
    }
}
