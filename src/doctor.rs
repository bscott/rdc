//! `rdc doctor`: is this machine ready to be served from / to serve?

use crate::desktop::Desktop;
use crate::desktop::local::LocalDesktop;
use crate::proto::*;
use crate::tailscale::Tailscale;

fn line(ok: Option<bool>, what: &str, detail: impl AsRef<str>) {
    let mark = match ok {
        Some(true) => "ok  ",
        Some(false) => "FAIL",
        None => "info",
    };
    println!("[{mark}] {what}: {}", detail.as_ref());
}

pub async fn run(request_permissions: bool) -> anyhow::Result<bool> {
    let mut healthy = true;
    line(None, "platform", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));
    let perms = if request_permissions { crate::permissions::request() } else { crate::permissions::check() };
    for (name, granted) in perms {
        if !granted {
            healthy = false;
        }
        line(
            Some(granted),
            "permission",
            format!(
                "{name}{}",
                if granted {
                    ""
                } else if request_permissions {
                    " — prompt shown; grant it in System Settings, then re-run"
                } else {
                    " — run `rdc doctor --request-permissions` from the GUI session"
                }
            ),
        );
    }
    #[cfg(target_os = "linux")]
    {
        let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        let de = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        line(None, "session", format!("{session} / {de}"));
        if session == "wayland" && de != "Hyprland" && de != "GNOME" && !de.contains("KDE") {
            line(None, "wayland", "untested compositor; capture uses portal Screenshot → wlr-screencopy");
        }
    }

    let ts = Tailscale::detect();
    line(Some(ts.available()), "tailscale", ts.describe());
    if ts.available() {
        match ts.self_ips().await {
            Ok(ips) if !ips.is_empty() => {
                line(Some(true), "tailscale ips", ips.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", "));
                match ts.whois(ips[0]).await {
                    Ok(id) => line(
                        Some(true),
                        "whois(self)",
                        format!("{} / {} / tags {:?}", id.login.as_deref().unwrap_or("-"), id.node, id.tags),
                    ),
                    Err(e) => {
                        healthy = false;
                        line(Some(false), "whois(self)", e.to_string());
                    }
                }
            }
            Ok(_) => {
                healthy = false;
                line(Some(false), "tailscale ips", "none (tailscaled up but not logged in?)");
            }
            Err(e) => {
                healthy = false;
                line(Some(false), "tailscale ips", e.to_string());
            }
        }
    } else {
        healthy = false;
    }

    let desk = match LocalDesktop::new() {
        Ok(d) => d,
        Err(e) => {
            line(Some(false), "desktop", e.to_string());
            return Ok(false);
        }
    };
    for d in desk.describe() {
        line(None, "backend", d);
    }
    match desk.displays().await {
        Ok(ds) if !ds.is_empty() => {
            for d in &ds {
                line(
                    Some(true),
                    "display",
                    format!(
                        "#{} {} {}x{} at ({},{}) scale {}{}",
                        d.id,
                        d.name,
                        d.rect.w,
                        d.rect.h,
                        d.rect.x,
                        d.rect.y,
                        d.scale,
                        if d.primary { " primary" } else { "" }
                    ),
                );
            }
        }
        Ok(_) => {
            healthy = false;
            line(Some(false), "display", "none found");
        }
        Err(e) => {
            healthy = false;
            line(Some(false), "display", e.to_string());
        }
    }
    let t0 = std::time::Instant::now();
    match desk
        .screenshot(ScreenshotReq { display: DisplayTarget::All, format: ImageFormat::Png, max_long_edge: Some(1568) })
        .await
    {
        Ok(s) => line(
            Some(true),
            "screenshot",
            format!(
                "{}x{} png {} KB in {:?} (covers {:?})",
                s.width,
                s.height,
                s.data.len() / 1024,
                t0.elapsed(),
                s.rect
            ),
        ),
        Err(e) => {
            healthy = false;
            line(
                Some(false),
                "screenshot",
                format!("{e} (macOS: grant Screen Recording; Wayland: portal/screencopy missing?)"),
            );
        }
    }
    let t0 = std::time::Instant::now();
    match desk.focused_window().await {
        Ok(w) => line(
            None,
            "focused window",
            format!(
                "{} in {:?} (looked up before every audited action; `[serve.audit].window_titles = false` to skip)",
                w.map(|w| crate::server::audit::describe_window(&w)).unwrap_or_else(|| "none".into()),
                t0.elapsed()
            ),
        ),
        Err(e) => line(None, "focused window", format!("{e}")),
    }
    match desk.windows().await {
        Ok(ws) => line(
            Some(true),
            "windows",
            format!(
                "{} listed{}",
                ws.len(),
                ws.iter()
                    .find(|w| w.focused)
                    .map(|w| format!(", focused: {} — {}", w.app, w.title))
                    .unwrap_or_default()
            ),
        ),
        Err(e) => line(Some(false), "windows", e.to_string()),
    }
    // A zero-length relative-ish probe: move the pointer to where it already is.
    match desk.input(InputAction::Key { chord: "shift".into() }).await {
        Ok(()) => line(Some(true), "input", "enigo initialised and a no-op key event was accepted"),
        Err(e) => {
            healthy = false;
            line(Some(false), "input", format!("{e} (macOS: grant Accessibility)"));
        }
    }
    match desk.clipboard_get().await {
        Ok(t) => line(Some(true), "clipboard", format!("readable ({} chars)", t.chars().count())),
        Err(e) => line(None, "clipboard", format!("{e}")),
    }
    line(None, "config", crate::config::path().display().to_string());
    match crate::config::insecure_reason(&crate::config::path()) {
        None => line(Some(true), "config perms", "owned by you, not writable by others"),
        Some(r) => {
            healthy = false;
            line(Some(false), "config perms", format!("{r}; `rdc serve` will refuse to start"));
        }
    }
    match crate::config::load() {
        Ok(cfg) => {
            match cfg.serve.grants() {
                Ok(gs) if gs.is_empty() => {
                    line(None, "allow", "empty — `rdc serve` will refuse to start until [serve].allow is set")
                }
                Ok(gs) => {
                    for g in gs {
                        line(Some(true), "allow", g.describe());
                    }
                }
                Err(e) => {
                    healthy = false;
                    line(Some(false), "allow", e.to_string());
                }
            }
            line(
                None,
                "audit",
                if cfg.serve.audit.enabled {
                    cfg.serve.audit.resolved_path().display().to_string()
                } else {
                    "disabled".into()
                },
            );
        }
        Err(e) => {
            healthy = false;
            line(Some(false), "config", e.to_string());
        }
    }
    Ok(healthy)
}
