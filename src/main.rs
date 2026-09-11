mod config;
mod desktop;
mod doctor;
mod keys;
mod mcp;
mod permissions;
mod proto;
mod server;
mod service;
mod tailscale;
mod view;
mod viewer;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use desktop::{Desktop, local::LocalDesktop, remote::RemoteDesktop};
use proto::*;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "rdc", version, about = "Remote Desktop Control for AI agents, over Tailscale")]
struct Cli {
    /// Machine to control: `local`, a name from config `[targets]`, a host[:port], or a URL.
    #[arg(short, long, global = true, default_value = "local", env = "RDC_TARGET")]
    target: String,
    /// Log filter (tracing syntax), e.g. `debug` or `rdc=debug,hyper=warn`.
    #[arg(long, global = true, default_value = "info", env = "RDC_LOG")]
    log: String,
    /// Append logs to this file instead of stderr (used by the Windows scheduled task).
    #[arg(long, global = true, env = "RDC_LOG_FILE")]
    log_file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon on this machine (the one to be controlled).
    Serve {
        /// Address to bind (default: this node's Tailscale IPv4).
        #[arg(long)]
        bind: Option<IpAddr>,
        /// TCP port to listen on (default 7770 or `[serve].port` in config).
        #[arg(long)]
        port: Option<u16>,
        /// Identity permitted to connect, optionally with capabilities: `alice@example.com`,
        /// `tag:ops=view`, `studio-laptop=view,clipboard`. Repeatable; adds to config.
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// Also stream audit entries (JSON lines) to this URL, e.g. an `rdc audit-view`
        /// instance: `http://laptop.example.ts.net:7771/v1/ingest`. Overrides config.
        #[arg(long)]
        audit_stream: Option<String>,
        /// Bind 127.0.0.1 and skip authentication for loopback. Testing only.
        #[arg(long)]
        dev_loopback: bool,
    },
    /// Collect audit entries streamed from `rdc serve` instances and show them live in a browser.
    AuditView {
        /// Address to bind (default: this node's Tailscale IPv4).
        #[arg(long)]
        bind: Option<IpAddr>,
        /// TCP port (default 7771 or `[audit_view].port`).
        #[arg(long)]
        port: Option<u16>,
        /// Identity allowed to send entries and to open the page. Repeatable; adds to config.
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// Keep received entries in this file (default: audit-view.jsonl in the state dir).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Keep entries in memory only.
        #[arg(long, conflicts_with = "store")]
        no_store: bool,
        /// Also follow a local audit file, e.g. this machine's own daemon log. Repeatable.
        #[arg(long)]
        follow: Vec<PathBuf>,
        /// Bind 127.0.0.1 and skip authentication for loopback. Testing only.
        #[arg(long)]
        dev_loopback: bool,
    },
    /// Run as an MCP stdio server for an agent, controlling --target.
    Mcp {
        /// Longest screenshot edge in pixels sent to the agent (default 1568).
        #[arg(long)]
        max: Option<u32>,
    },
    /// Check this machine's readiness to serve or to reach tailscaled.
    Doctor {
        /// macOS: trigger the Screen Recording / Accessibility prompts if not yet granted.
        #[arg(long)]
        request_permissions: bool,
    },
    /// Install, remove or inspect the background service that runs `rdc serve`.
    Service {
        #[arg(value_enum)]
        op: ServiceOp,
        /// Windows: run the daemon elevated (highest run level) so it can click and type into
        /// elevated windows. The default is a standard-integrity task. Ignored elsewhere.
        #[arg(long)]
        elevated: bool,
    },
    /// Show the most recent entries of this machine's audit log.
    Audit {
        /// Number of entries to show.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Print raw JSON lines instead of a table.
        #[arg(long)]
        json: bool,
        /// Audit file (default: `[serve.audit].path` or the platform state dir).
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// List displays.
    Displays,
    /// List windows.
    Windows,
    /// Take a screenshot.
    Shot {
        /// `all` (every monitor composited), `primary`, or a display id from `displays`.
        #[arg(long, default_value = "all")]
        display: DisplayTarget,
        /// Downscale so the longer edge is at most this many pixels.
        #[arg(long)]
        max: Option<u32>,
        /// Encode as JPEG (smaller) instead of PNG.
        #[arg(long)]
        jpeg: bool,
        /// Output file (default: shot-<timestamp>.<ext>).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Move the pointer.
    Move { x: i32, y: i32 },
    /// Click at a point.
    Click {
        x: i32,
        y: i32,
        /// left, right or middle.
        #[arg(long, default_value = "left")]
        button: MouseButton,
        /// Double-click instead of single.
        #[arg(long)]
        double: bool,
    },
    /// Drag from one point to another.
    Drag {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[arg(long, default_value = "left")]
        button: MouseButton,
    },
    /// Scroll (positive dy = down, positive dx = right), optionally at a point.
    Scroll {
        /// Horizontal wheel steps; positive scrolls right.
        #[arg(long, default_value_t = 0)]
        dx: i32,
        /// Vertical wheel steps; positive scrolls down.
        #[arg(long, default_value_t = 0)]
        dy: i32,
        /// Move the pointer here first (desktop points).
        #[arg(long, num_args = 2, value_names = ["X", "Y"])]
        at: Option<Vec<i32>>,
    },
    /// Type literal text.
    Type { text: String },
    /// Press a key chord like `cmd+shift+4`, `ctrl+c`, `enter`.
    Key { chord: String },
    /// Focus a window by id, app name or title substring.
    Focus {
        /// Window id from `windows`.
        #[arg(long, conflicts_with_all = ["app", "title"])]
        id: Option<u64>,
        /// Case-insensitive substring of the application name.
        #[arg(long)]
        app: Option<String>,
        /// Case-insensitive substring of the window title.
        #[arg(long)]
        title: Option<String>,
    },
    /// Read the clipboard, or set it when TEXT is given.
    Clip { text: Option<String> },
    /// Ask the target daemon who it thinks we are.
    Whoami,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ServiceOp {
    Install,
    Uninstall,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // enigo logs its own connection attempts at ERROR (we deliberately point unused backends at
    // invalid endpoints); real input failures still surface as returned errors.
    let filter = tracing_subscriber::EnvFilter::try_new(format!("{},enigo=off", cli.log))
        .unwrap_or_else(|_| "info,enigo=off".into());
    let to_file = cli.log_file.is_some();
    match &cli.log_file {
        Some(p) => {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            let mut opts = std::fs::OpenOptions::new();
            opts.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let file = opts.open(p).with_context(|| format!("opening log file {}", p.display()))?;
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init(),
    }
    let result = run(cli).await;
    if let Err(e) = &result
        && to_file
    {
        // Otherwise a headless launch (Windows scheduled task) dies with nothing in the log.
        tracing::error!("fatal: {e:#}");
    }
    result
}

async fn run(cli: Cli) -> Result<()> {
    #[cfg(target_os = "windows")]
    desktop::local::set_dpi_aware();
    let cfg = config::load()?;

    match cli.cmd {
        Cmd::Serve { bind, port, allow, audit_stream, dev_loopback } => {
            config::enforce_permissions()?;
            let desktop: Arc<dyn Desktop> = Arc::new(LocalDesktop::new()?);
            let ts = tailscale::Tailscale::detect();
            let mut grants = cfg.serve.grants()?;
            for a in &allow {
                grants.push(config::parse_allow_flag(a)?);
            }
            let bind = bind.or_else(|| cfg.serve.bind.as_deref().and_then(|s| s.parse().ok()));
            let mut audit = cfg.serve.audit.clone();
            if audit_stream.is_some() {
                audit.stream = audit_stream;
            }
            server::serve(
                desktop,
                ts,
                server::ServeOpts {
                    bind,
                    port: port.unwrap_or(cfg.serve.port),
                    grants,
                    audit,
                    hosts: cfg.serve.hosts.clone(),
                    dev_loopback,
                },
            )
            .await
        }
        Cmd::AuditView { bind, port, allow, store, no_store, follow, dev_loopback } => {
            config::enforce_permissions()?;
            let ts = tailscale::Tailscale::detect();
            let v = &cfg.audit_view;
            let mut grants = v.grants()?;
            for a in &allow {
                grants.push(config::parse_allow_flag(a)?);
            }
            let bind = bind.or_else(|| v.bind.as_deref().and_then(|s| s.parse().ok()));
            let store = if no_store {
                None
            } else {
                let mut c = v.store_config();
                if let Some(p) = store {
                    c.path = Some(p);
                }
                Some(c)
            };
            let mut follow = follow;
            follow.extend(v.follow.iter().cloned());
            viewer::run(
                ts,
                viewer::ViewOpts {
                    bind,
                    port: port.unwrap_or(v.port),
                    grants,
                    hosts: v.hosts.clone(),
                    dev_loopback,
                    store,
                    follow,
                },
            )
            .await
        }
        Cmd::Mcp { max } => {
            let desktop: Arc<dyn Desktop> = match cfg.resolve_target(&cli.target)? {
                config::TargetKind::Local => Arc::new(LocalDesktop::new()?),
                config::TargetKind::Url(u) => Arc::new(RemoteDesktop::new(&u)?),
            };
            mcp::run(desktop, cli.target.clone(), max).await
        }
        Cmd::Service { op, elevated } => {
            if matches!(op, ServiceOp::Install) {
                config::enforce_permissions()?;
            }
            if matches!(op, ServiceOp::Install) && cfg.serve.grants()?.is_empty() {
                anyhow::bail!(
                    "[serve].allow in {} is empty; the service would start `rdc serve` with nobody allowed and exit. \
                     Add at least one tailnet login, node name or tag there first.",
                    config::path().display()
                );
            }
            if elevated && (!cfg!(windows) || !matches!(op, ServiceOp::Install)) {
                eprintln!("warning: --elevated only applies to `service install` on Windows; ignoring");
            }
            service::run(match op {
                ServiceOp::Install => service::Op::Install { elevated },
                ServiceOp::Uninstall => service::Op::Uninstall,
                ServiceOp::Status => service::Op::Status,
            })
        }
        Cmd::Audit { lines, json, path } => {
            let path = path.unwrap_or_else(|| cfg.serve.audit.resolved_path());
            let entries = server::audit::tail(&path, lines).with_context(|| format!("reading {}", path.display()))?;
            if json {
                for e in &entries {
                    println!("{}", serde_json::to_string(e)?);
                }
            } else {
                println!("{:<24} {:<28} {:<7} {:<4} action", "time", "who", "outcome", "code");
                for e in &entries {
                    let clean = server::audit::sanitize;
                    let who = clean(&e.login.clone().or(e.node.clone()).unwrap_or_else(|| e.peer.clone()));
                    let what = clean(&e.action.clone().unwrap_or_else(|| format!("{} {}", e.method, e.path)));
                    let detail = e.detail.as_deref().map(|d| format!("  ({})", clean(d))).unwrap_or_default();
                    println!("{:<24} {:<28} {:<7} {:<4} {what}{detail}", e.ts, who, e.outcome, e.status);
                }
                if entries.is_empty() {
                    eprintln!("no entries in {}", path.display());
                }
            }
            Ok(())
        }
        Cmd::Doctor { request_permissions } => {
            let ok = doctor::run(request_permissions).await?;
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        other => client(&cfg, &cli.target, other).await,
    }
}

async fn client(cfg: &config::Config, target: &str, cmd: Cmd) -> Result<()> {
    let (desktop, remote): (Arc<dyn Desktop>, Option<Arc<RemoteDesktop>>) = match cfg.resolve_target(target)? {
        config::TargetKind::Local => (Arc::new(LocalDesktop::new()?), None),
        config::TargetKind::Url(u) => {
            let r = Arc::new(RemoteDesktop::new(&u)?);
            (r.clone(), Some(r))
        }
    };
    match cmd {
        Cmd::Displays => print_json(&desktop.displays().await?),
        Cmd::Windows => print_json(&desktop.windows().await?),
        Cmd::Shot { display, max, jpeg, out } => {
            let format = if jpeg { ImageFormat::Jpeg } else { ImageFormat::Png };
            let s = desktop.screenshot(ScreenshotReq { display, format, max_long_edge: max }).await?;
            let path = out.unwrap_or_else(|| {
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                PathBuf::from(format!("shot-{t}.{}", format.ext()))
            });
            std::fs::write(&path, &s.data).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("{} {}x{} covers desktop {:?}", path.display(), s.width, s.height, s.rect);
            println!("{}", path.display());
            Ok(())
        }
        Cmd::Move { x, y } => desktop.input(InputAction::MouseMove { x, y }).await.map_err(Into::into),
        Cmd::Click { x, y, button, double } => desktop
            .input(InputAction::Click { x, y, button, count: if double { 2 } else { 1 } })
            .await
            .map_err(Into::into),
        Cmd::Drag { x1, y1, x2, y2, button } => {
            desktop.input(InputAction::Drag { from: (x1, y1), to: (x2, y2), button }).await.map_err(Into::into)
        }
        Cmd::Scroll { dx, dy, at } => {
            desktop.input(InputAction::Scroll { at: at.map(|v| (v[0], v[1])), dx, dy }).await.map_err(Into::into)
        }
        Cmd::Type { text } => desktop.input(InputAction::Type { text }).await.map_err(Into::into),
        Cmd::Key { chord } => {
            keys::parse_chord(&chord)?;
            desktop.input(InputAction::Key { chord }).await.map_err(Into::into)
        }
        Cmd::Focus { id, app, title } => {
            let t = match (id, app, title) {
                (Some(i), _, _) => WindowTarget::Id(i),
                (_, Some(a), _) => WindowTarget::App(a),
                (_, _, Some(t)) => WindowTarget::Title(t),
                _ => anyhow::bail!("give one of --id, --app, --title"),
            };
            desktop.focus(t).await.map_err(Into::into)
        }
        Cmd::Clip { text: Some(t) } => desktop.clipboard_set(t).await.map_err(Into::into),
        Cmd::Clip { text: None } => {
            println!("{}", desktop.clipboard_get().await?);
            Ok(())
        }
        Cmd::Whoami => match remote {
            Some(r) => print_json(&r.whoami().await?),
            None => anyhow::bail!("whoami needs a remote --target"),
        },
        Cmd::Serve { .. }
        | Cmd::AuditView { .. }
        | Cmd::Mcp { .. }
        | Cmd::Doctor { .. }
        | Cmd::Service { .. }
        | Cmd::Audit { .. } => {
            unreachable!()
        }
    }
}

fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}
