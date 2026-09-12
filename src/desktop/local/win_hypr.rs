//! Hyprland window/monitor management through `hyprctl -j`, because xcap cannot see
//! Wayland-native windows and there is no cross-compositor way to focus one.

use crate::proto::*;
use serde::Deserialize;
use std::process::Command;

pub struct Hyprland;

#[derive(Deserialize)]
struct HMon {
    id: u32,
    name: String,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    scale: f32,
    transform: u8,
    focused: bool,
}

#[derive(Deserialize)]
struct HClient {
    address: String,
    pid: i64,
    class: String,
    title: String,
    at: (i32, i32),
    size: (i32, i32),
    mapped: bool,
    hidden: bool,
    #[serde(rename = "focusHistoryID")]
    focus_history_id: i64,
}

impl Hyprland {
    pub fn detect() -> Option<Self> {
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        Command::new("hyprctl").arg("version").output().ok().filter(|o| o.status.success()).map(|_| Hyprland)
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out =
            Command::new("hyprctl").args(args).output().map_err(|e| RdcError::Backend(format!("hyprctl: {e}")))?;
        if !out.status.success() {
            return Err(RdcError::Backend(format!(
                "hyprctl {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn displays(&self) -> Result<Vec<Display>> {
        let raw = self.run(&["-j", "monitors"])?;
        let mons: Vec<HMon> =
            serde_json::from_str(&raw).map_err(|e| RdcError::Backend(format!("hyprctl monitors json: {e}")))?;
        Ok(mons
            .into_iter()
            .map(|m| {
                // Odd transforms are 90/270° rotations: swap logical width/height.
                let (pw, ph) = if m.transform % 2 == 1 { (m.height, m.width) } else { (m.width, m.height) };
                let s = if m.scale > 0.0 { m.scale } else { 1.0 };
                Display {
                    id: m.id,
                    name: m.name,
                    rect: Rect { x: m.x, y: m.y, w: (pw as f32 / s).round() as u32, h: (ph as f32 / s).round() as u32 },
                    scale: s,
                    primary: m.focused,
                }
            })
            .collect())
    }

    pub fn windows(&self) -> Result<Vec<Window>> {
        let raw = self.run(&["-j", "clients"])?;
        let cs: Vec<HClient> =
            serde_json::from_str(&raw).map_err(|e| RdcError::Backend(format!("hyprctl clients json: {e}")))?;
        let mut out: Vec<Window> = cs
            .into_iter()
            .filter(|c| c.mapped)
            .map(|c| Window {
                id: u64::from_str_radix(c.address.trim_start_matches("0x"), 16).unwrap_or(0),
                pid: c.pid.max(0) as u32,
                app: c.class,
                title: c.title,
                rect: Rect { x: c.at.0, y: c.at.1, w: c.size.0.max(0) as u32, h: c.size.1.max(0) as u32 },
                focused: c.focus_history_id == 0,
                minimized: c.hidden,
            })
            .collect();
        out.sort_by_key(|w| !w.focused);
        Ok(out)
    }

    /// `hyprctl activewindow` returns just the focused client (or an empty object when nothing
    /// is focused), so this avoids serialising the whole client list per action.
    pub fn focused_window(&self) -> Result<Option<Window>> {
        let raw = self.run(&["-j", "activewindow"])?;
        if raw.trim().is_empty() || raw.trim() == "{}" {
            return Ok(None);
        }
        let c: HClient =
            serde_json::from_str(&raw).map_err(|e| RdcError::Backend(format!("hyprctl activewindow json: {e}")))?;
        if !c.mapped {
            return Ok(None);
        }
        Ok(Some(Window {
            id: u64::from_str_radix(c.address.trim_start_matches("0x"), 16).unwrap_or(0),
            pid: c.pid.max(0) as u32,
            app: c.class,
            title: c.title,
            rect: Rect { x: c.at.0, y: c.at.1, w: c.size.0.max(0) as u32, h: c.size.1.max(0) as u32 },
            focused: true,
            minimized: c.hidden,
        }))
    }

    pub fn focus(&self, w: &Window) -> Result<()> {
        self.run(&["dispatch", "focuswindow", &format!("address:0x{:x}", w.id)]).map(|_| ())
    }
}
