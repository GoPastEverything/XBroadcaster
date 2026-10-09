//! Media path for XBroadcaster.
//!
//! Capture sleeps inside DXGI until the desktop changes. The GPU scales that
//! frame to the stream size. Encoding starts only while a broadcast is going
//! out, and the encoder queue is one frame deep: a late frame is dropped, not
//! buffered.

mod audio;
mod capture;
mod color;
mod encode;
mod pipeline;
mod rtmp;
mod text;

pub use pipeline::{self_check, CheckReport, OutputControl, Pipeline};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("{0}")]
    Message(String),
    #[error("windows: {0}")]
    Windows(#[from] windows::core::Error),
}

impl MediaError {
    pub fn message(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }
}
