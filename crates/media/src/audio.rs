//! Microphone and desktop-loopback capture.
//!
//! WASAPI shared mode with autoconvert delivers float stereo at the stream
//! sample rate, so this process does not resample. Each capture thread blocks
//! on a driver event. Peaks are published for the meters and the mixer pulls
//! only what the current video frame needs.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use wasapi::{
    AudioCaptureClient, AudioClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode,
    WaveFormat, initialize_mta,
};

use crate::MediaError;

const MAX_FRAMES: usize = 48_000;

pub struct AudioMixer {
    mic: Arc<PcmQueue>,
    desktop: Arc<PcmQueue>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    mic_peak: Arc<AtomicU32>,
    desktop_peak: Arc<AtomicU32>,
}

struct PcmQueue {
    samples: Mutex<VecDeque<f32>>,
}

impl AudioMixer {
    pub fn start(sample_rate: u32) -> Result<Self, MediaError> {
        let stop = Arc::new(AtomicBool::new(false));
        let mic = Arc::new(PcmQueue {
            samples: Mutex::new(VecDeque::new()),
        });
        let desktop = Arc::new(PcmQueue {
            samples: Mutex::new(VecDeque::new()),
        });
        let mic_peak = Arc::new(AtomicU32::new(0));
        let desktop_peak = Arc::new(AtomicU32::new(0));
        let mut threads = Vec::new();

        let mic_queue = Arc::clone(&mic);
        let mic_stop = Arc::clone(&stop);
        let mic_meter = Arc::clone(&mic_peak);
        threads.push(std::thread::Builder::new().name("xb-mic".into()).spawn(move || {
            if let Err(err) = capture_loop(Direction::Capture, false, sample_rate, mic_queue, mic_stop, mic_meter) {
                eprintln!("microphone: {err}");
            }
        }).map_err(|err| MediaError::message(format!("mic thread: {err}")))?);

        let desk_queue = Arc::clone(&desktop);
        let desk_stop = Arc::clone(&stop);
        let desk_meter = Arc::clone(&desktop_peak);
        threads.push(std::thread::Builder::new().name("xb-desktop-audio".into()).spawn(move || {
            if let Err(err) = capture_loop(Direction::Render, true, sample_rate, desk_queue, desk_stop, desk_meter) {
                eprintln!("desktop audio: {err}");
            }
        }).map_err(|err| MediaError::message(format!("desktop audio thread: {err}")))?);

        Ok(Self {
            mic,
            desktop,
            stop,
            threads,
            mic_peak,
            desktop_peak,
        })
    }

    /// Mix `frames` of stereo f32. Missing input becomes silence so the video
    /// clock never waits on audio.
    pub fn pull(&self, frames: usize, mic_gain: f32, desktop_gain: f32, mic_muted: bool, desktop_muted: bool) -> Vec<f32> {
        let mut mic = take(&self.mic, frames);
        let mut desktop = take(&self.desktop, frames);
        if mic.len() < frames * 2 {
            mic.resize(frames * 2, 0.0);
        }
        if desktop.len() < frames * 2 {
            desktop.resize(frames * 2, 0.0);
        }
        let mic_gain = if mic_muted { 0.0 } else { mic_gain.clamp(0.0, 2.0) };
        let desktop_gain = if desktop_muted { 0.0 } else { desktop_gain.clamp(0.0, 2.0) };
        let mut mixed = vec![0.0f32; frames * 2];
        let mut mic_peak = 0.0f32;
        let mut desk_peak = 0.0f32;
        for index in 0..frames * 2 {
            mic_peak = mic_peak.max(mic[index].abs());
            desk_peak = desk_peak.max(desktop[index].abs());
            let sample = mic[index] * mic_gain + desktop[index] * desktop_gain;
            mixed[index] = sample.clamp(-1.0, 1.0);
        }
        store_decay(&self.mic_peak, mic_peak);
        store_decay(&self.desktop_peak, desk_peak);
        mixed
    }

    pub fn peaks(&self) -> (f32, f32) {
        (
            f32::from_bits(self.mic_peak.load(Ordering::Relaxed)),
            f32::from_bits(self.desktop_peak.load(Ordering::Relaxed)),
        )
    }
}

impl Drop for AudioMixer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn take(queue: &PcmQueue, frames: usize) -> Vec<f32> {
    let mut guard = queue.samples.lock().unwrap_or_else(|err| err.into_inner());
    let count = (frames * 2).min(guard.len());
    guard.drain(..count).collect()
}

fn store_decay(slot: &AtomicU32, peak: f32) {
    let previous = f32::from_bits(slot.load(Ordering::Relaxed));
    let next = peak.max(previous * 0.85);
    slot.store(next.to_bits(), Ordering::Relaxed);
}

fn capture_loop(
    device_direction: Direction,
    loopback: bool,
    sample_rate: u32,
    queue: Arc<PcmQueue>,
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU32>,
) -> Result<(), MediaError> {
    let _ = initialize_mta();
    let enumerator = DeviceEnumerator::new().map_err(|err| MediaError::message(err.to_string()))?;
    let device = enumerator
        .get_default_device(&device_direction)
        .map_err(|err| MediaError::message(err.to_string()))?;
    let mut client = device
        .get_iaudioclient()
        .map_err(|err| MediaError::message(err.to_string()))?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, sample_rate as usize, 2, None);
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 200_000,
    };
    let init_direction = if loopback { Direction::Capture } else { device_direction };
    client
        .initialize_client(&format, &init_direction, &mode)
        .map_err(|err| MediaError::message(err.to_string()))?;
    let event = client
        .set_get_eventhandle()
        .map_err(|err| MediaError::message(err.to_string()))?;
    let capture = client
        .get_audiocaptureclient()
        .map_err(|err| MediaError::message(err.to_string()))?;
    client.start_stream().map_err(|err| MediaError::message(err.to_string()))?;
    let mut local = VecDeque::new();
    while !stop.load(Ordering::Relaxed) {
        if event.wait_for_event(200).is_err() {
            continue;
        }
        if capture.read_from_device_to_deque(&mut local).is_err() {
            break;
        }
        let mut chunk = Vec::new();
        while local.len() >= 8 {
            let mut bytes = [0u8; 4];
            for byte in &mut bytes {
                *byte = local.pop_front().unwrap();
            }
            chunk.push(f32::from_le_bytes(bytes));
            for byte in &mut bytes {
                *byte = local.pop_front().unwrap();
            }
            chunk.push(f32::from_le_bytes(bytes));
        }
        if !chunk.is_empty() {
            let level = chunk.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
            store_decay(&peak, level);
            let mut guard = queue.samples.lock().unwrap_or_else(|err| err.into_inner());
            guard.extend(chunk);
            let overflow = guard.len().saturating_sub(MAX_FRAMES * 2);
            if overflow > 0 {
                guard.drain(..overflow);
            }
        }
    }
    let _ = client.stop_stream();
    let _ = (event, capture);
    Ok(())
}

/// Silence unused warnings if a future edit stops naming these directly.
#[allow(dead_code)]
fn _types(client: &AudioClient, capture: &AudioCaptureClient, event: &Handle) {
    let _ = (client, capture, event);
}

pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|sample| (sample.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}
