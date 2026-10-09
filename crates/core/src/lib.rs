//! Scene model, program compositor, and the shared state the studio UI and
//! the media pipeline both see.
//!
//! The compositor runs at the stream size (1280x720 by default). It never
//! scales a 4K desktop on the CPU. Display frames arrive already reduced to
//! the canvas.

mod composite;
mod project;
mod shared;

pub use composite::{composite, Canvas, FrameView};
pub use project::{
    AudioSettings, BroadcastSettings, OutputSettings, Project, Scene, Source, SourceKind,
    CHAT_EVERYONE, CHAT_FOLLOWED, CHAT_OFF, CHAT_SUBSCRIBERS, CHAT_VERIFIED,
};
pub use shared::{
    load_peak, store_peak, AudioLevels, LiveStatus, PipelineStats, PreviewFrame, SharedState,
};
