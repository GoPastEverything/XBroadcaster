use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::Project;

#[derive(Clone, Debug)]
pub struct PreviewFrame {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub generation: u64,
}

impl Default for PreviewFrame {
    fn default() -> Self {
        Self {
            bgra: Vec::new(),
            width: 0,
            height: 0,
            generation: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PipelineStats {
    pub capture_ms: f32,
    pub compose_ms: f32,
    pub convert_ms: f32,
    pub encode_ms: f32,
    pub capture_fps: f32,
    pub output_fps: f32,
    pub dropped_frames: u64,
    pub idle_skips: u64,
    pub encoder_name: String,
    pub hardware_encoder: bool,
    pub output: String,
    pub monitors: Vec<String>,
}

impl Default for PipelineStats {
    fn default() -> Self {
        Self {
            capture_ms: 0.0,
            compose_ms: 0.0,
            convert_ms: 0.0,
            encode_ms: 0.0,
            capture_fps: 0.0,
            output_fps: 0.0,
            dropped_frames: 0,
            idle_skips: 0,
            encoder_name: "not started".to_string(),
            hardware_encoder: false,
            output: "preview".to_string(),
            monitors: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct LiveStatus {
    pub phase: String,
    pub detail: String,
    pub share_url: Option<String>,
    pub broadcast_id: Option<String>,
    pub on_air: bool,
    pub error: Option<String>,
}

/// Studio-wide state. The UI writes the project. The pipeline reads it when
/// `project_gen` changes and publishes preview frames back.
pub struct SharedState {
    pub project: Mutex<Project>,
    pub project_gen: AtomicU64,
    pub preview: Mutex<PreviewFrame>,
    pub stats: Mutex<PipelineStats>,
    pub live: Mutex<LiveStatus>,
    pub mic_peak_bits: AtomicU32,
    pub desktop_peak_bits: AtomicU32,
    pub output_connected: AtomicBool,
    pub output_error: Mutex<Option<String>>,
    pub shutdown: AtomicBool,
}

impl SharedState {
    pub fn new(project: Project) -> Arc<Self> {
        Arc::new(Self {
            project: Mutex::new(project),
            project_gen: AtomicU64::new(1),
            preview: Mutex::new(PreviewFrame::default()),
            stats: Mutex::new(PipelineStats::default()),
            live: Mutex::new(LiveStatus {
                phase: "Idle".to_string(),
                detail: "Sign in with X, then go live.".to_string(),
                ..LiveStatus::default()
            }),
            mic_peak_bits: AtomicU32::new(0),
            desktop_peak_bits: AtomicU32::new(0),
            output_connected: AtomicBool::new(false),
            output_error: Mutex::new(None),
            shutdown: AtomicBool::new(false),
        })
    }

    pub fn touch_project(&self) {
        self.project_gen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn lock_project(&self) -> std::sync::MutexGuard<'_, Project> {
        self.project.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub fn set_live(&self, live: LiveStatus) {
        *self.live.lock().unwrap_or_else(|err| err.into_inner()) = live;
    }

    pub fn live_snapshot(&self) -> LiveStatus {
        self.live.lock().unwrap_or_else(|err| err.into_inner()).clone()
    }

    pub fn stats_snapshot(&self) -> PipelineStats {
        self.stats.lock().unwrap_or_else(|err| err.into_inner()).clone()
    }
}

pub fn store_peak(slot: &AtomicU32, peak: f32) {
    slot.store(peak.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

pub fn load_peak(slot: &AtomicU32) -> f32 {
    f32::from_bits(slot.load(Ordering::Relaxed)).clamp(0.0, 1.0)
}

impl AudioLevels {
    pub fn mic(&self) -> f32 {
        load_peak(&self.mic)
    }

    pub fn desktop(&self) -> f32 {
        load_peak(&self.desktop)
    }
}

pub struct AudioLevels {
    pub mic: AtomicU32,
    pub desktop: AtomicU32,
}
