use serde::{Deserialize, Serialize};

pub const CHAT_OFF: u8 = 1;
pub const CHAT_EVERYONE: u8 = 2;
pub const CHAT_VERIFIED: u8 = 3;
pub const CHAT_FOLLOWED: u8 = 4;
pub const CHAT_SUBSCRIBERS: u8 = 5;

/// Studio project. Saved as JSON under `%APPDATA%\XBroadcaster`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub scenes: Vec<Scene>,
    pub active_scene: usize,
    pub output: OutputSettings,
    pub audio: AudioSettings,
    pub broadcast: BroadcastSettings,
    next_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Bits per second. X's recommended starting point for 720p30 is 4_000_000.
    pub video_bitrate: u32,
    pub audio_bitrate: u32,
    pub keyframe_seconds: u32,
    /// AAC sample rate. 44100 matches X's recommended source configuration.
    pub sample_rate: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioSettings {
    pub mic_gain: f32,
    pub desktop_gain: f32,
    pub mic_muted: bool,
    pub desktop_muted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BroadcastSettings {
    pub title: String,
    pub low_latency: bool,
    pub chat_option: u8,
    /// When false, X does not create an announcement post on publish.
    pub post_announcement: bool,
    pub source_name: String,
    /// Persistent X stream source. The source id is also the RTMPS stream key,
    /// so this file should stay in the user profile.
    pub saved_source_id: Option<String>,
    pub saved_region: Option<String>,
    /// RTMPS ingest from X Live Studio. Used when the Livestream API is locked.
    /// The stream key stays in this profile file and is never logged.
    #[serde(default)]
    pub manual_rtmps_url: String,
    #[serde(default)]
    pub manual_stream_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scene {
    pub id: u64,
    pub name: String,
    pub sources: Vec<Source>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub id: u64,
    pub name: String,
    pub kind: SourceKind,
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub opacity: f32,
}

/// BGRA colors are stored as `[b, g, r, a]`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceKind {
    Display { monitor: usize },
    Color { color: [u8; 4] },
    Text { text: String, color: [u8; 4], px: u32 },
}

impl Default for Project {
    fn default() -> Self {
        let mut project = Self {
            scenes: Vec::new(),
            active_scene: 0,
            output: OutputSettings {
                width: 1280,
                height: 720,
                fps: 30,
                video_bitrate: 4_000_000,
                audio_bitrate: 128_000,
                keyframe_seconds: 3,
                sample_rate: 44_100,
            },
            audio: AudioSettings {
                mic_gain: 1.0,
                desktop_gain: 0.7,
                mic_muted: false,
                desktop_muted: false,
            },
            broadcast: BroadcastSettings {
                title: "Live on X".to_string(),
                low_latency: true,
                chat_option: CHAT_EVERYONE,
                post_announcement: true,
                source_name: "XBroadcaster".to_string(),
                saved_source_id: None,
                saved_region: None,
                manual_rtmps_url: String::new(),
                manual_stream_key: String::new(),
            },
            next_id: 1,
        };
        let main = project.alloc_scene("Main");
        project.add_source(
            main,
            "Display",
            SourceKind::Display { monitor: 0 },
            0,
            0,
            1280,
            720,
        );
        let brb = project.alloc_scene("Starting soon");
        project.add_source(
            brb,
            "Backdrop",
            SourceKind::Color {
                color: [16, 16, 18, 255],
            },
            0,
            0,
            1280,
            720,
        );
        project.add_source(
            brb,
            "Title",
            SourceKind::Text {
                text: "Starting soon".to_string(),
                color: [255, 255, 255, 255],
                px: 72,
            },
            280,
            300,
            720,
            120,
        );
        project
    }
}

impl Project {
    pub fn active(&self) -> Option<&Scene> {
        self.scenes.get(self.active_scene)
    }

    pub fn active_mut(&mut self) -> Option<&mut Scene> {
        self.scenes.get_mut(self.active_scene)
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    pub fn alloc_scene(&mut self, name: &str) -> u64 {
        let id = self.alloc_id();
        self.scenes.push(Scene {
            id,
            name: name.to_string(),
            sources: Vec::new(),
        });
        id
    }

    pub fn add_source(
        &mut self,
        scene_id: u64,
        name: &str,
        kind: SourceKind,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Option<u64> {
        let id = self.alloc_id();
        let scene = self.scenes.iter_mut().find(|scene| scene.id == scene_id)?;
        scene.sources.push(Source {
            id,
            name: name.to_string(),
            kind,
            visible: true,
            x,
            y,
            w,
            h,
            opacity: 1.0,
        });
        Some(id)
    }

    pub fn remove_active_source(&mut self, source_id: u64) {
        if let Some(scene) = self.active_mut() {
            scene.sources.retain(|source| source.id != source_id);
        }
    }
}

impl OutputSettings {
    pub fn frame_duration(&self) -> std::time::Duration {
        let fps = self.fps.max(1);
        std::time::Duration::from_nanos(1_000_000_000 / u64::from(fps))
    }
}
