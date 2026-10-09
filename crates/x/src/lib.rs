//! X account login and the Livestream API.
//!
//! Access to these endpoints is whitelist-only. The broadcasting user signs in
//! with OAuth 2.0 PKCE (`broadcast.read`, `broadcast.write`, `users.read`,
//! `offline.access`). A stream source is created once and reused. A broadcast
//! can be created only after the RTMPS ingest is already receiving video.

mod client;
mod oauth;

pub use client::{CreatedBroadcast, StreamSource, XClient};
pub use oauth::{sign_in, AppConfig, Session, XError};

use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("XBroadcaster")
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_json<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<(), XError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| XError::message(err.to_string()))?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|err| XError::message(err.to_string()))?;
    std::fs::write(path, text).map_err(|err| XError::message(err.to_string()))
}
