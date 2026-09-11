//! The daemon: axum on the Tailscale IP, every request identified via whois.

pub mod audit;
pub mod auth;
mod routes;
pub mod stream;
#[cfg(test)]
mod tests;

use crate::config::{AuditConfig, ResolvedGrant};
use crate::desktop::Desktop;
use crate::tailscale::Tailscale;
use anyhow::{Context, Result};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

pub use auth::Allowlist;

#[derive(Clone)]
pub struct AppState {
    pub desktop: Arc<dyn Desktop>,
    pub auth: Arc<auth::Auth>,
    pub audit: Arc<audit::Audit>,
}

impl auth::Gate for AppState {
    fn auth(&self) -> &auth::Auth {
        &self.auth
    }
    fn record(&self, e: audit::Entry) {
        self.audit.record(e);
    }
}

/// Where a listener goes and which `Host` names it answers to. Shared by `serve` and the audit
/// viewer so both apply the same rules: Tailscale addresses only, loopback only for testing.
pub struct Listen {
    pub addr: SocketAddr,
    pub hosts: Vec<String>,
    /// This node's short name, for stamping into audit entries.
    pub node: String,
}

pub async fn plan_listen(
    ts: &Tailscale,
    bind: Option<IpAddr>,
    port: u16,
    dev_loopback: bool,
    extra_hosts: Vec<String>,
) -> Result<Listen> {
    let bind_ip = match bind {
        Some(ip) => ip,
        None if dev_loopback => IpAddr::from([127, 0, 0, 1]),
        None => {
            let ips = ts.self_ips().await.context("tailscaled unreachable; pass --bind to choose an address")?;
            *ips.first().context("this node has no Tailscale IP; is tailscaled up?")?
        }
    };
    if dev_loopback && !bind_ip.is_loopback() {
        anyhow::bail!("--dev-loopback only allows a loopback bind address, not {bind_ip}");
    }
    if !dev_loopback && !is_tailscale_ip(bind_ip) {
        anyhow::bail!(
            "{bind_ip} is not a Tailscale address; refusing to listen on it (use --dev-loopback for 127.0.0.1)"
        );
    }
    // Names a legitimate client would put in the URL. Anything else in the Host header means the
    // request was not addressed to us (e.g. a browser lured by DNS rebinding) and is refused.
    let mut hosts: Vec<String> = vec![bind_ip.to_string()];
    if let Ok(ips) = ts.self_ips().await {
        hosts.extend(ips.iter().map(|ip| ip.to_string()));
    }
    let names = ts.self_names().await;
    let node = names.last().cloned().unwrap_or_else(|| bind_ip.to_string());
    hosts.extend(names);
    hosts.extend(extra_hosts);
    if dev_loopback {
        hosts.extend(["localhost".to_string(), "127.0.0.1".to_string(), "::1".to_string()]);
    }
    tracing::debug!(?hosts, "accepted Host names");
    Ok(Listen { addr: SocketAddr::new(bind_ip, port), hosts, node })
}

pub struct ServeOpts {
    pub bind: Option<IpAddr>,
    pub port: u16,
    pub grants: Vec<ResolvedGrant>,
    pub audit: AuditConfig,
    /// Extra `Host` names to accept besides this node's own addresses and names.
    pub hosts: Vec<String>,
    pub dev_loopback: bool,
}

pub async fn serve(desktop: Arc<dyn Desktop>, ts: Tailscale, opts: ServeOpts) -> Result<()> {
    // On macOS this pops the Screen Recording / Accessibility prompts on the console the first
    // time; screenshots show only the wallpaper until the user grants and we are restarted.
    for (name, granted) in crate::permissions::request() {
        if granted {
            tracing::info!("permission {name}: granted");
        } else {
            tracing::warn!(
                "permission {name}: NOT granted; approve it in System Settings > Privacy & Security, then restart rdc"
            );
        }
    }
    let listen = plan_listen(&ts, opts.bind, opts.port, opts.dev_loopback, opts.hosts).await?;
    if opts.grants.is_empty() && !opts.dev_loopback {
        anyhow::bail!("allowlist is empty: set [serve].allow in config or pass --allow; nobody could connect");
    }
    for g in &opts.grants {
        tracing::info!("allow {}", g.describe());
    }
    let mut audit = audit::Audit::open(&opts.audit, &listen.node).context("opening the audit log")?;
    match audit.path() {
        Some(p) => tracing::info!("audit log: {}", p.display()),
        None => tracing::warn!("audit log disabled by config"),
    }
    if let Some(url) = &opts.audit.stream {
        audit.stream_to(url, opts.audit.stream_token.as_deref()).context("audit stream")?;
        tracing::info!("audit stream: {url}");
    }
    let addr = listen.addr;
    let auth = auth::Auth::new(
        Arc::new(ts),
        Allowlist::new(opts.grants),
        auth::HostAllow::new(listen.hosts),
        opts.dev_loopback,
    );
    if opts.dev_loopback {
        tracing::warn!("--dev-loopback: requests from 127.0.0.1 are NOT authenticated");
    }
    let audit = Arc::new(audit);
    let state = AppState { desktop, auth: Arc::new(auth), audit: audit.clone() };
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("bind {addr}"))?;
    tracing::info!("rdc serving on http://{addr}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    audit.shutdown().await;
    Ok(())
}

/// 100.64.0.0/10 (CGNAT range Tailscale uses) or fd7a:115c:a1e0::/48.
pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..128).contains(&o[1])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}
