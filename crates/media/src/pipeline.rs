//! One thread owns capture, compose, encode, and the RTMPS socket.
//!
//! While idle it blocks in Desktop Duplication and does not composite. While
//! live it emits one frame per tick and drops a tick instead of building a
//! queue. That is the opposite of the backlog that turns a short stall into
//! "encoder overloaded".

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_NOSOCKET};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use xb_core::{
    FrameView, Project, Scene, SharedState, SourceKind, composite,
    store_peak, Canvas,
};

use crate::audio::{f32_to_i16, AudioMixer};
use crate::capture::{DisplayCapture, MonitorInfo};
use crate::encode::{asc_for, AudioEncoder, VideoEncoder};
use crate::rtmp::RtmpPublisher;
use crate::text::{rasterize, TextBitmap, TextKey};
use crate::MediaError;

pub struct Pipeline {
    tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

/// Send-able handle so the go-live thread can start and stop the encoder
/// without sharing the pipeline thread.
#[derive(Clone)]
pub struct OutputControl {
    tx: Sender<Command>,
}

enum Command {
    StartOutput { url: String, key: String },
    StopOutput,
    Shutdown,
}

impl Pipeline {
    pub fn start(shared: Arc<SharedState>) -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("xb-pipeline".into())
            .spawn(move || {
                if let Err(err) = run(shared, rx) {
                    eprintln!("pipeline stopped: {err}");
                }
            })
            .expect("pipeline thread");
        Self {
            tx,
            thread: Some(thread),
        }
    }

    pub fn control(&self) -> OutputControl {
        OutputControl { tx: self.tx.clone() }
    }
}

impl OutputControl {
    pub fn start_output(&self, url: String, key: String) {
        let _ = self.tx.send(Command::StartOutput { url, key });
    }

    pub fn stop_output(&self) {
        let _ = self.tx.send(Command::StopOutput);
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Output {
    video: VideoEncoder,
    audio: AudioEncoder,
    publisher: RtmpPublisher,
    frame_index: u64,
    samples_sent: u64,
    headers_sent: bool,
    pcm: Vec<i16>,
}

fn run(shared: Arc<SharedState>, rx: Receiver<Command>) -> Result<(), MediaError> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(mf_version(), MFSTARTUP_NOSOCKET)?;
    }
    let monitors = DisplayCapture::list_monitors().unwrap_or_default();
    {
        let mut stats = shared.stats.lock().unwrap_or_else(|err| err.into_inner());
        stats.monitors = monitors.iter().map(monitor_label).collect();
    }

    let sample_rate = shared.lock_project().output.sample_rate;
    let audio = match AudioMixer::start(sample_rate) {
        Ok(mixer) => Some(mixer),
        Err(err) => {
            eprintln!("audio: {err}");
            None
        }
    };

    let mut capturer: Option<DisplayCapture> = None;
    let mut capture_monitor = usize::MAX;
    let mut canvas = Canvas::new(1280, 720);
    let mut texts: HashMap<u64, TextBitmap> = HashMap::new();
    let mut display: Option<Vec<u8>> = None;
    let mut display_size = (0u32, 0u32);
    let mut seen_gen = 0u64;
    let mut output: Option<Output> = None;
    let mut next_tick = Instant::now();
    let mut stats_at = Instant::now();
    let mut preview_at = Instant::now();
    let mut captures = 0u32;
    let mut outputs = 0u32;
    let mut capture_ms = 0.0f32;
    let mut compose_ms = 0.0f32;
    let mut encode_ms = 0.0f32;

    loop {
        if shared.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        while let Ok(command) = rx.try_recv() {
            match command {
                Command::Shutdown => {
                    output.take();
                    unsafe { let _ = MFShutdown(); }
                    return Ok(());
                }
                Command::StopOutput => {
                    output.take();
                    shared.output_connected.store(false, std::sync::atomic::Ordering::Relaxed);
                    set_output(&shared, "preview");
                }
                Command::StartOutput { url, key } => {
                    output.take();
                    shared.output_connected.store(false, std::sync::atomic::Ordering::Relaxed);
                    match open_output(&shared, &url, &key, &canvas) {
                        Ok(session) => {
                            output = Some(session);
                            next_tick = Instant::now();
                            set_output(&shared, "connecting");
                        }
                        Err(err) => {
                            *shared.output_error.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(err.to_string());
                            set_output(&shared, "output error");
                        }
                    }
                }
            }
        }

        let project = shared.lock_project().clone();
        let gen = shared.project_gen.load(std::sync::atomic::Ordering::Relaxed);
        canvas.resize(project.output.width, project.output.height);
        let scene = project.active().cloned();
        let display_source = scene.as_ref().and_then(|scene| {
            scene.sources.iter().find(|source| {
                source.visible && matches!(source.kind, SourceKind::Display { .. })
            })
        });

        let mut frame_changed = false;
        if let Some(source) = display_source {
            let monitor = match source.kind {
                SourceKind::Display { monitor } => monitor,
                _ => 0,
            };
            if capturer.is_none() || capture_monitor != monitor {
                match DisplayCapture::start(monitor, project.output.width, project.output.height) {
                    Ok(next) => {
                        capturer = Some(next);
                        capture_monitor = monitor;
                    }
                    Err(err) => {
                        capturer = None;
                        set_output(&shared, &format!("capture: {err}"));
                    }
                }
            }
            if let Some(capturer) = capturer.as_mut() {
                let timeout = if output.is_some() { 2 } else { 40 };
                let started = Instant::now();
                match capturer.frame(timeout) {
                    Ok(Some(frame)) => {
                        display = Some(frame.bgra.to_vec());
                        display_size = (frame.width, frame.height);
                        frame_changed = true;
                        captures += 1;
                        capture_ms = started.elapsed().as_secs_f32() * 1000.0;
                    }
                    Ok(None) => {}
                    Err(err) => set_output(&shared, &format!("capture: {err}")),
                }
            }
        } else {
            capturer = None;
            capture_monitor = usize::MAX;
        }

        let scene_changed = gen != seen_gen;
        if scene_changed {
            seen_gen = gen;
            texts.retain(|id, _| {
                scene.as_ref().map(|scene| scene.sources.iter().any(|source| source.id == *id)).unwrap_or(false)
            });
        }
        let streaming = output.is_some();
        let compose_now = frame_changed || scene_changed || streaming;
        if !compose_now {
            let mut stats = shared.stats.lock().unwrap_or_else(|err| err.into_inner());
            stats.idle_skips = stats.idle_skips.saturating_add(1);
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }

        if let Some(scene) = scene.as_ref() {
            let started = Instant::now();
            refresh_text(scene, &mut texts);
            let mut views = Vec::new();
            for source in &scene.sources {
                match &source.kind {
                    SourceKind::Display { .. } => {
                        if let Some(pixels) = display.as_deref() {
                            views.push(FrameView {
                                source_id: source.id,
                                bgra: pixels,
                                width: display_size.0,
                                height: display_size.1,
                                stride: display_size.0 as usize * 4,
                            });
                        }
                    }
                    SourceKind::Text { .. } => {
                        if let Some(bitmap) = texts.get(&source.id) {
                            views.push(FrameView {
                                source_id: source.id,
                                bgra: &bitmap.bgra,
                                width: bitmap.width,
                                height: bitmap.height,
                                stride: bitmap.width as usize * 4,
                            });
                        }
                    }
                    SourceKind::Color { .. } => {}
                }
            }
            composite(&mut canvas, scene, &views);
            compose_ms = started.elapsed().as_secs_f32() * 1000.0;
        }

        if preview_at.elapsed() >= Duration::from_millis(66) {
            publish_preview(&shared, &canvas);
            preview_at = Instant::now();
        }

        if let Some(session) = output.as_mut() {
            let now = Instant::now();
            if now + Duration::from_millis(1) < next_tick {
                std::thread::sleep(next_tick.saturating_duration_since(now));
            } else if now > next_tick + project.output.frame_duration() {
                let missed = (now.saturating_duration_since(next_tick).as_nanos()
                    / project.output.frame_duration().as_nanos().max(1)) as u64;
                if missed > 1 {
                    let mut stats = shared.stats.lock().unwrap_or_else(|err| err.into_inner());
                    stats.dropped_frames = stats.dropped_frames.saturating_add(missed - 1);
                    session.frame_index = session.frame_index.saturating_add(missed - 1);
                }
                next_tick = now;
            }
            if let Err(err) = tick_output(session, &project, &canvas, audio.as_ref(), &mut encode_ms) {
                *shared.output_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(err.to_string());
                set_output(&shared, "output error");
                output = None;
                shared.output_connected.store(false, std::sync::atomic::Ordering::Relaxed);
            } else {
                outputs += 1;
                if !shared.output_connected.load(std::sync::atomic::Ordering::Relaxed) {
                    shared.output_connected.store(true, std::sync::atomic::Ordering::Relaxed);
                    set_output(&shared, "sending");
                }
            }
            next_tick += project.output.frame_duration();
        }

        if let Some(mixer) = audio.as_ref() {
            let (mic, desktop) = mixer.peaks();
            store_peak(&shared.mic_peak_bits, mic);
            store_peak(&shared.desktop_peak_bits, desktop);
        }
        if stats_at.elapsed() >= Duration::from_secs(1) {
            let secs = stats_at.elapsed().as_secs_f32().max(0.001);
            let mut stats = shared.stats.lock().unwrap_or_else(|err| err.into_inner());
            stats.capture_ms = capture_ms;
            stats.compose_ms = compose_ms;
            stats.encode_ms = encode_ms;
            stats.capture_fps = captures as f32 / secs;
            stats.output_fps = outputs as f32 / secs;
            if let Some(session) = output.as_ref() {
                stats.encoder_name = session.video.name.clone();
                stats.hardware_encoder = session.video.hardware;
            }
            captures = 0;
            outputs = 0;
            stats_at = Instant::now();
        }
    }
    unsafe {
        let _ = MFShutdown();
    }
    Ok(())
}

fn open_output(shared: &SharedState, url: &str, key: &str, canvas: &Canvas) -> Result<Output, MediaError> {
    let project = shared.lock_project().clone();
    *shared.output_error.lock().unwrap_or_else(|err| err.into_inner()) = None;
    let video = VideoEncoder::open(
        project.output.width,
        project.output.height,
        project.output.fps,
        project.output.video_bitrate,
    )?;
    let audio = AudioEncoder::open(project.output.sample_rate, project.output.audio_bitrate)?;
    let mut stats = shared.stats.lock().unwrap_or_else(|err| err.into_inner());
    stats.encoder_name = video.name.clone();
    stats.hardware_encoder = video.hardware;
    drop(stats);
    let publisher = RtmpPublisher::connect(url, key)?;
    let _ = canvas;
    Ok(Output {
        video,
        audio,
        publisher,
        frame_index: 0,
        samples_sent: 0,
        headers_sent: false,
        pcm: Vec::new(),
    })
}

fn tick_output(
    session: &mut Output,
    project: &Project,
    canvas: &Canvas,
    mixer: Option<&AudioMixer>,
    encode_ms: &mut f32,
) -> Result<(), MediaError> {
    session.publisher.poll()?;
    let frames = (project.output.sample_rate / project.output.fps.max(1)) as usize;
    if let Some(mixer) = mixer {
        let mixed = mixer.pull(
            frames,
            project.audio.mic_gain,
            project.audio.desktop_gain,
            project.audio.mic_muted,
            project.audio.desktop_muted,
        );
        session.pcm.extend(f32_to_i16(&mixed));
    }
    let started = Instant::now();
    let pts = session.frame_index as i64 * 10_000_000 / i64::from(project.output.fps.max(1));
    let video = session.video.encode(&canvas.bgra, pts)?;
    *encode_ms = started.elapsed().as_secs_f32() * 1000.0;
    let audio_frames = session.audio.push(&session.pcm)?;
    session.pcm.clear();

    if !session.headers_sent {
        if let Some(config) = session.video.config().map(|config| config.to_vec()) {
            session.publisher.write_metadata(
                project.output.width,
                project.output.height,
                project.output.fps,
                project.output.video_bitrate / 1000,
                project.output.sample_rate,
                project.output.audio_bitrate / 1000,
            )?;
            session.publisher.write_aac_header(&asc_for(project.output.sample_rate))?;
            session.publisher.write_avc_header(&config)?;
            session.headers_sent = true;
        }
    }
    if session.headers_sent {
        let mut audio_pts = (session.samples_sent * 1000 / u64::from(project.output.sample_rate.max(1))) as u32;
        for frame in audio_frames {
            session.publisher.write_aac(audio_pts, &frame)?;
            session.samples_sent += 1024;
            audio_pts = (session.samples_sent * 1000 / u64::from(project.output.sample_rate.max(1))) as u32;
        }
        let video_pts = (session.frame_index * 1000 / u64::from(project.output.fps.max(1))) as u32;
        for frame in video {
            session.publisher.write_video(video_pts, frame.keyframe, &frame.avcc)?;
        }
    }
    session.frame_index += 1;
    Ok(())
}

fn refresh_text(scene: &Scene, cache: &mut HashMap<u64, TextBitmap>) {
    for source in &scene.sources {
        if let SourceKind::Text { text, color, px } = &source.kind {
            let key = TextKey {
                text: text.clone(),
                px: *px,
                color: *color,
            };
            let stale = cache.get(&source.id).map(|bitmap| bitmap.key != key).unwrap_or(true);
            if stale {
                if let Ok(bitmap) = rasterize(text, *px, *color) {
                    cache.insert(source.id, bitmap);
                }
            }
        }
    }
}

fn publish_preview(shared: &SharedState, canvas: &Canvas) {
    let mut rgba = vec![0u8; canvas.bgra.len()];
    for (src, dst) in canvas.bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = 255;
    }
    let mut preview = shared.preview.lock().unwrap_or_else(|err| err.into_inner());
    preview.generation = preview.generation.wrapping_add(1);
    preview.width = canvas.width;
    preview.height = canvas.height;
    preview.bgra = rgba;
}

fn set_output(shared: &SharedState, text: &str) {
    shared.stats.lock().unwrap_or_else(|err| err.into_inner()).output = text.to_string();
}

fn monitor_label(info: &MonitorInfo) -> String {
    format!("{} ({}x{})", info.name, info.width, info.height)
}

fn mf_version() -> u32 {
    // MF_VERSION in the Windows headers: 0x0002 << 16 | 0x0070 for the Vista+ constant
    // used by MFStartup. windows-rs also exports MF_VERSION; call it if the const path differs.
    windows::Win32::Media::MediaFoundation::MF_VERSION
}

pub struct CheckReport {
    pub monitors: Vec<String>,
    pub capture_ms: f32,
    pub captured: bool,
    pub encoder: String,
    pub hardware: bool,
    pub encode_ms: f32,
    pub nal_bytes: usize,
    pub audio: String,
    pub notes: Vec<String>,
}

pub fn self_check() -> CheckReport {
    let mut notes = Vec::new();
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() && hr.0 != 0 {
            notes.push(format!("COM: {hr:?}"));
        }
        if let Err(err) = MFStartup(mf_version(), MFSTARTUP_NOSOCKET) {
            notes.push(format!("Media Foundation: {err}"));
        }
    }
    let monitors = DisplayCapture::list_monitors().unwrap_or_else(|err| {
        notes.push(format!("monitor list: {err}"));
        Vec::new()
    });
    let mut capture_ms = 0.0;
    let mut captured = false;
    let mut pixels = vec![0u8; 1280 * 720 * 4];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[32, 32, 36, 255]);
    }
    match DisplayCapture::start(0, 1280, 720) {
        Ok(mut capturer) => {
            let started = Instant::now();
            match capturer.frame(500) {
                Ok(Some(frame)) => {
                    capture_ms = started.elapsed().as_secs_f32() * 1000.0;
                    captured = true;
                    pixels = frame.bgra.to_vec();
                }
                Ok(None) => {
                    // A second wait catches a desktop that did not present during the first call.
                    if let Ok(Some(frame)) = capturer.frame(500) {
                        capture_ms = started.elapsed().as_secs_f32() * 1000.0;
                        captured = true;
                        pixels = frame.bgra.to_vec();
                    } else {
                        notes.push("desktop did not present a new frame during the check".into());
                    }
                }
                Err(err) => notes.push(format!("capture: {err}")),
            }
        }
        Err(err) => notes.push(format!("capture open: {err}")),
    }

    let mut encoder = "unavailable".to_string();
    let mut hardware = false;
    let mut encode_ms = 0.0;
    let mut nal_bytes = 0usize;
    match VideoEncoder::open(1280, 720, 30, 4_000_000) {
        Ok(mut video) => {
            encoder = video.name.clone();
            hardware = video.hardware;
            let started = Instant::now();
            // A hardware encoder can spend the first call renegotiating its
            // output type and only return picture NALs on a later frame.
            let mut encode_error = None;
            for index in 0..4 {
                match video.encode(&pixels, index * 333_333) {
                    Ok(frames) => {
                        nal_bytes += frames.iter().map(|frame| frame.avcc.len()).sum::<usize>();
                        if nal_bytes > 0 && video.config().is_some() {
                            break;
                        }
                    }
                    Err(err) => {
                        encode_error = Some(err);
                        break;
                    }
                }
            }
            encode_ms = started.elapsed().as_secs_f32() * 1000.0;
            if let Some(err) = encode_error {
                notes.push(format!("encode: {err}"));
            } else if nal_bytes == 0 {
                notes.push("encode: encoder accepted frames but returned no NALs".into());
            } else {
                // Startup includes format negotiation. This loop is the rate
                // that has to fit in a frame (33 ms at 30 fps).
                let warmed = Instant::now();
                let mut steady_frames = 0u32;
                let mut convert_ms = 0.0f32;
                let mut submit_ms = 0.0f32;
                for index in 0..12 {
                    match video.encode(&pixels, 2_000_000 + index * 333_333) {
                        Ok(frames) => {
                            nal_bytes += frames.iter().map(|frame| frame.avcc.len()).sum::<usize>();
                            convert_ms += video.last_convert_ms;
                            submit_ms += video.last_submit_ms;
                            steady_frames += 1;
                        }
                        Err(err) => {
                            notes.push(format!("encode: steady-state frame failed: {err}"));
                            break;
                        }
                    }
                }
                if steady_frames > 0 {
                    let n = steady_frames as f32;
                    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
                    notes.push(format!(
                        "steady: {steady_frames} frames, {:.2} ms each ({profile}; color {:.2} ms, encoder {:.2} ms)",
                        warmed.elapsed().as_secs_f32() * 1000.0 / n,
                        convert_ms / n,
                        submit_ms / n
                    ));
                }
            }
        }
        Err(err) => notes.push(format!("encoder: {err}")),
    }

    let audio = match AudioMixer::start(44_100) {
        Ok(mixer) => {
            std::thread::sleep(Duration::from_millis(120));
            let samples = mixer.pull(1024, 1.0, 1.0, false, false);
            let energy = samples.iter().map(|sample| sample.abs()).sum::<f32>();
            format!("running, {:.0} mixed samples, energy {energy:.3}", samples.len())
        }
        Err(err) => {
            notes.push(format!("audio: {err}"));
            "failed".into()
        }
    };
    unsafe {
        let _ = MFShutdown();
    }
    CheckReport {
        monitors: monitors.iter().map(monitor_label).collect(),
        capture_ms,
        captured,
        encoder,
        hardware,
        encode_ms,
        nal_bytes,
        audio,
        notes,
    }
}
