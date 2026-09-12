//! The `Desktop` abstraction: everything the MCP server and CLI can do to a machine.
//! Implemented in-process by [`local::LocalDesktop`] and over HTTP by [`remote::RemoteDesktop`].

pub mod local;
pub mod remote;

use crate::proto::*;
use async_trait::async_trait;

#[async_trait]
pub trait Desktop: Send + Sync {
    async fn displays(&self) -> Result<Vec<Display>>;
    async fn screenshot(&self, req: ScreenshotReq) -> Result<Screenshot>;
    async fn windows(&self) -> Result<Vec<Window>>;
    async fn focus(&self, target: WindowTarget) -> Result<()>;
    async fn input(&self, action: InputAction) -> Result<()>;
    async fn clipboard_get(&self) -> Result<String>;
    async fn clipboard_set(&self, text: String) -> Result<()>;

    /// The window that currently has focus, for audit context. This is not a new capability
    /// (it is a subset of [`Desktop::windows`]) and is not exposed on the wire; it exists so the
    /// daemon can record "what was in front" per action without paying for a full enumeration.
    /// Backends override it with a direct query where the platform has one.
    async fn focused_window(&self) -> Result<Option<Window>> {
        Ok(self.windows().await?.into_iter().find(|w| w.focused))
    }

    async fn state(&self) -> Result<State> {
        Ok(State { displays: self.displays().await?, windows: self.windows().await? })
    }

    async fn act(&self, action: Action) -> Result<()> {
        match action {
            Action::Input(a) => self.input(a).await,
            Action::Focus(t) => self.focus(t).await,
            Action::ClipboardSet { text } => self.clipboard_set(text).await,
        }
    }
}

/// Pick a window matching `target` from a list (shared by backends).
pub fn find_window<'a>(windows: &'a [Window], target: &WindowTarget) -> Option<&'a Window> {
    match target {
        WindowTarget::Id(id) => windows.iter().find(|w| w.id == *id),
        WindowTarget::App(a) => {
            let a = a.to_lowercase();
            windows.iter().find(|w| w.app.to_lowercase().contains(&a))
        }
        WindowTarget::Title(t) => {
            let t = t.to_lowercase();
            windows.iter().find(|w| w.title.to_lowercase().contains(&t))
        }
    }
}
