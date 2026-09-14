//! Screen and window capture via xcap, plus compositing and downscaling.

use crate::proto::*;
use image::{RgbaImage, imageops::FilterType};
use std::io::Cursor;
use xcap::Monitor;

pub fn backend_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "CoreGraphics"
    } else if cfg!(target_os = "windows") {
        "GDI/WGC"
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "wayland (portal → wlr-screencopy)"
    } else {
        "x11"
    }
}

fn xe(e: xcap::XCapError) -> RdcError {
    RdcError::Backend(format!("xcap: {e}"))
}

/// Displays as xcap sees them. On most platforms these are already in the coordinate
/// space the input layer expects; Hyprland overrides this with `hyprctl` geometry.
pub fn displays() -> Result<Vec<Display>> {
    let mons = Monitor::all().map_err(xe)?;
    let mut out = Vec::with_capacity(mons.len());
    for m in mons {
        out.push(Display {
            id: m.id().map_err(xe)?,
            name: m.name().map_err(xe)?,
            rect: Rect {
                x: m.x().map_err(xe)?,
                y: m.y().map_err(xe)?,
                w: m.width().map_err(xe)?,
                h: m.height().map_err(xe)?,
            },
            scale: m.scale_factor().map_err(xe)?,
            primary: m.is_primary().map_err(xe)?,
        });
    }
    Ok(out)
}

/// Read one xcap window into our shape. Every accessor here is a separate query into the
/// platform's window server — on macOS each one re-runs `CGWindowListCopyWindowInfo` over the
/// whole desktop — so call this for as few windows as the caller actually needs.
/// `focused` is passed in because the caller often already knows it more cheaply than
/// `is_focused()` can work it out.
fn to_window(w: &xcap::Window, focused: bool) -> Result<Option<Window>> {
    let title = w.title().unwrap_or_default();
    let app = w.app_name().unwrap_or_default();
    if title.is_empty() && app.is_empty() {
        return Ok(None);
    }
    Ok(Some(Window {
        id: w.id().map_err(xe)? as u64,
        pid: w.pid().unwrap_or(0),
        app,
        title,
        rect: Rect {
            x: w.x().unwrap_or(0),
            y: w.y().unwrap_or(0),
            w: w.width().unwrap_or(0),
            h: w.height().unwrap_or(0),
        },
        focused,
        minimized: w.is_minimized().unwrap_or(false),
    }))
}

pub fn windows() -> Result<Vec<Window>> {
    let ws = xcap::Window::all().map_err(xe)?;
    let mut out = Vec::new();
    for w in ws {
        let focused = w.is_focused().unwrap_or(false);
        if let Some(win) = to_window(&w, focused)? {
            out.push(win);
        }
    }
    Ok(out)
}

/// The frontmost window belonging to `pid`, reading the accessors for that window only.
///
/// xcap lists windows front to back, so the match is normally the first entry and the cost is
/// one window-list query plus the accessors for the single hit, whatever the window count.
#[cfg(target_os = "macos")]
pub fn window_of_pid(pid: u32) -> Result<Option<Window>> {
    for w in xcap::Window::all().map_err(xe)? {
        if w.pid().unwrap_or(0) == pid {
            return to_window(&w, true);
        }
    }
    Ok(None)
}

/// Find the xcap monitor that corresponds to one of our logical displays.
fn monitor_for(d: &Display) -> Result<Monitor> {
    let mons = Monitor::all().map_err(xe)?;
    // Prefer id, then name, then a point inside the display.
    if let Some(m) = mons.iter().find(|m| m.id().ok() == Some(d.id)) {
        return Ok(m.clone());
    }
    if let Some(m) = mons.iter().find(|m| m.name().ok().as_deref() == Some(d.name.as_str())) {
        return Ok(m.clone());
    }
    Monitor::from_point(d.rect.x + 1, d.rect.y + 1).map_err(xe)
}

fn capture_display(d: &Display) -> Result<RgbaImage> {
    let t0 = std::time::Instant::now();
    #[cfg(target_os = "macos")]
    {
        // CGWindowListCreateImage is shimmed through ScreenCaptureKit on recent macOS and takes
        // seconds per call; the system `screencapture` tool uses SCK directly and is fast.
        match screencapture_cli(d) {
            Ok(img) => {
                tracing::debug!("screencapture {}x{} in {:?}", img.width(), img.height(), t0.elapsed());
                return Ok(img);
            }
            Err(e) => tracing::warn!("screencapture failed ({e}); falling back to xcap"),
        }
    }
    let img = monitor_for(d)?.capture_image().map_err(xe)?;
    tracing::debug!("xcap capture {}x{} in {:?}", img.width(), img.height(), t0.elapsed());
    Ok(img)
}

/// `screencapture -x -D <n>` where n is the 1-based display index in xcap's monitor order.
#[cfg(target_os = "macos")]
fn screencapture_cli(d: &Display) -> Result<RgbaImage> {
    let mons = Monitor::all().map_err(xe)?;
    let idx = mons
        .iter()
        .position(|m| m.id().ok() == Some(d.id))
        .or_else(|| mons.iter().position(|m| m.name().ok().as_deref() == Some(d.name.as_str())))
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "rdc-shot-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    let out = std::process::Command::new("/usr/sbin/screencapture")
        .args(["-x", "-t", "png", "-D", &(idx + 1).to_string()])
        .arg(&path)
        .output()
        .map_err(|e| RdcError::Backend(format!("screencapture: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(RdcError::Backend(format!(
            "screencapture exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let img = image::open(&path).map_err(|e| RdcError::Backend(format!("decode screencapture png: {e}")))?.into_rgba8();
    let _ = std::fs::remove_file(&path);
    Ok(img)
}

pub fn screenshot(displays: &[Display], req: &ScreenshotReq) -> Result<Screenshot> {
    if displays.is_empty() {
        return Err(RdcError::Backend("no displays found".into()));
    }
    let chosen: Vec<&Display> = match req.display {
        DisplayTarget::All => displays.iter().collect(),
        DisplayTarget::Primary => vec![displays.iter().find(|d| d.primary).unwrap_or(&displays[0])],
        DisplayTarget::Id(id) => {
            vec![displays.iter().find(|d| d.id == id).ok_or_else(|| RdcError::NotFound(format!("display {id}")))?]
        }
    };

    let (mut img, rect) =
        if chosen.len() == 1 { (capture_display(chosen[0])?, chosen[0].rect) } else { composite(&chosen)? };

    if let Some(max) = req.max_long_edge {
        let long = img.width().max(img.height());
        if max > 0 && long > max {
            let f = max as f64 / long as f64;
            let nw = ((img.width() as f64 * f).round() as u32).max(1);
            let nh = ((img.height() as f64 * f).round() as u32).max(1);
            img = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
        }
    }

    let (width, height) = (img.width(), img.height());
    let t_enc = std::time::Instant::now();
    let data = encode(img, req.format)?;
    tracing::debug!("encoded {width}x{height} {:?} ({} KB) in {:?}", req.format, data.len() / 1024, t_enc.elapsed());
    Ok(Screenshot { format: req.format, width, height, rect, data })
}

/// Stitch several displays into one image at the primary display's pixel density.
fn composite(displays: &[&Display]) -> Result<(RgbaImage, Rect)> {
    let union = displays.iter().skip(1).fold(displays[0].rect, |acc, d| acc.union(&d.rect));
    let scale = displays.iter().find(|d| d.primary).unwrap_or(&displays[0]).scale.max(0.5);
    let px = |v: f64| (v * scale as f64).round() as u32;
    let mut canvas = RgbaImage::new(px(union.w as f64).max(1), px(union.h as f64).max(1));
    for d in displays {
        let mut img = capture_display(d)?;
        let (tw, th) = (px(d.rect.w as f64).max(1), px(d.rect.h as f64).max(1));
        if img.width() != tw || img.height() != th {
            img = image::imageops::resize(&img, tw, th, FilterType::Triangle);
        }
        let ox = px((d.rect.x - union.x) as f64) as i64;
        let oy = px((d.rect.y - union.y) as f64) as i64;
        image::imageops::overlay(&mut canvas, &img, ox, oy);
    }
    Ok((canvas, union))
}

fn encode(img: RgbaImage, fmt: ImageFormat) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    match fmt {
        ImageFormat::Png => {
            use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
            let enc = PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, PngFilter::Adaptive);
            img.write_with_encoder(enc)
        }
        ImageFormat::Jpeg => {
            use image::codecs::jpeg::JpegEncoder;
            let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
            rgb.write_with_encoder(JpegEncoder::new_with_quality(&mut buf, 85))
        }
    }
    .map_err(|e| RdcError::Backend(format!("encode: {e}")))?;
    Ok(buf.into_inner())
}
