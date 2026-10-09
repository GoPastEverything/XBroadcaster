//! H.264 and AAC through Windows Media Foundation.
//!
//! Hardware transforms (NVENC, Quick Sync, AMF) are preferred. A software
//! transform is used only when the machine has no hardware encoder. Frames are
//! NV12 at the stream size. The desktop-size image never reaches this stage.

use std::mem::size_of;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::Interface;

use crate::color::bgra_to_nv12;
use crate::rtmp::{annexb_nals, avc_decoder_config, length_prefixed, nal_type};
use crate::MediaError;

pub struct VideoFrame {
    pub keyframe: bool,
    pub avcc: Vec<u8>,
}

pub struct VideoEncoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    nv12: Vec<u8>,
    out_buf: Vec<u8>,
    avcc: Option<Vec<u8>>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    pub name: String,
    pub hardware: bool,
    pub last_convert_ms: f32,
    pub last_submit_ms: f32,
    provides_samples: bool,
    asynchronous: bool,
    alignment: u32,
}

pub struct AudioEncoder {
    transform: IMFTransform,
    sample_rate: u32,
    pending: Vec<i16>,
    pub name: String,
}

pub fn asc_for(sample_rate: u32) -> [u8; 2] {
    let index = match sample_rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        _ => 4,
    };
    let object_type = 2u8; // AAC-LC
    let channels = 2u8;
    [
        (object_type << 3) | (index >> 1),
        ((index & 1) << 7) | (channels << 3),
    ]
}

impl VideoEncoder {
    pub fn open(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self, MediaError> {
        let width = width.max(2) & !1;
        let height = height.max(2) & !1;
        let fps = fps.max(1);
        unsafe {
            let (transform, name, hardware, asynchronous) = open_h264(width, height, fps, bitrate)?;
            let info = transform.GetOutputStreamInfo(0)?;
            let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let alignment = info.cbAlignment.max(16);
            let frame_bytes = (width as usize) * (height as usize);
            let out_len = (info.cbSize as usize).max(frame_bytes * 2).max(64 * 1024);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            Ok(Self {
                transform,
                width,
                height,
                fps,
                nv12: vec![0u8; frame_bytes * 3 / 2],
                out_buf: vec![0u8; out_len],
                avcc: None,
                sps: Vec::new(),
                pps: Vec::new(),
                name,
                hardware,
                last_convert_ms: 0.0,
                last_submit_ms: 0.0,
                provides_samples,
                asynchronous,
                alignment,
            })
        }
    }

    pub fn config(&self) -> Option<&[u8]> {
        self.avcc.as_deref()
    }

    pub fn encode(&mut self, bgra: &[u8], pts_100ns: i64) -> Result<Vec<VideoFrame>, MediaError> {
        let started = std::time::Instant::now();
        bgra_to_nv12(bgra, self.width as usize, self.height as usize, &mut self.nv12);
        self.last_convert_ms = started.elapsed().as_secs_f32() * 1000.0;
        let submit = std::time::Instant::now();
        let frames = unsafe {
            if self.asynchronous {
                self.submit_async(pts_100ns)
            } else {
                self.submit_and_drain(pts_100ns)
            }
        };
        self.last_submit_ms = submit.elapsed().as_secs_f32() * 1000.0;
        frames
    }

    /// Intel Quick Sync (and some other hardware encoders) are asynchronous MFTs.
    /// They only accept input after `METransformNeedInput` and only produce bytes
    /// after `METransformHaveOutput`.
    unsafe fn submit_async(&mut self, pts: i64) -> Result<Vec<VideoFrame>, MediaError> {
        unsafe {
            let len = self.nv12.len() as u32;
            let buffer = MFCreateAlignedMemoryBuffer(len, self.alignment)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(10_000_000 / i64::from(self.fps.max(1)))?;
            let events: IMFMediaEventGenerator = self.transform.cast().map_err(|err| {
                MediaError::message(format!("encoder events: {err}"))
            })?;
            let mut frames = Vec::new();
            let mut sent = false;
            // The first frame waits out encoder startup. Later frames are dropped
            // rather than stalling the compositor if the encoder falls behind.
            let budget_ms = if self.avcc.is_some() { 40 } else { 1000 };
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
            while std::time::Instant::now() < deadline {
                match events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                        if sent && !frames.is_empty() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(err) => return Err(MediaError::message(format!("encoder event: {err}"))),
                    Ok(event) => {
                        let kind = event.GetType()?;
                        if kind == MEError.0 as u32 {
                            let status = event.GetStatus().unwrap_or(windows::core::HRESULT(0x8000FFFF_u32 as _));
                            return Err(MediaError::message(format!("encoder error event {status:?}")));
                        }
                        if kind == METransformNeedInput.0 as u32 && !sent {
                            self.transform.ProcessInput(0, &sample, 0).map_err(|err| {
                                MediaError::message(format!("process input: {err}"))
                            })?;
                            sent = true;
                        } else if kind == METransformHaveOutput.0 as u32 {
                            frames.extend(self.drain()?);
                            if sent && !frames.is_empty() {
                                break;
                            }
                        }
                    }
                }
            }
            if !sent {
                return Err(MediaError::message("hardware encoder did not request a frame"));
            }
            Ok(frames)
        }
    }

    unsafe fn submit_and_drain(&mut self, pts: i64) -> Result<Vec<VideoFrame>, MediaError> {
        unsafe {
            let len = self.nv12.len() as u32;
            let buffer = MFCreateMemoryBuffer(len)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(10_000_000 / i64::from(self.fps.max(1)))?;
            self.transform.ProcessInput(0, &sample, 0)?;
            self.drain()
        }
    }

    unsafe fn drain(&mut self) -> Result<Vec<VideoFrame>, MediaError> {
        unsafe {
            let mut frames = Vec::new();
            let mut format_changes = 0u32;
            if let Ok(info) = self.transform.GetOutputStreamInfo(0) {
                self.provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
                if info.cbAlignment > 0 {
                    self.alignment = info.cbAlignment.max(16);
                }
            }
            loop {
                let mut output = MFT_OUTPUT_DATA_BUFFER::default();
                if !self.provides_samples {
                    let buffer = MFCreateAlignedMemoryBuffer(self.out_buf.len() as u32, self.alignment)?;
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&buffer)?;
                    output.pSample = std::mem::ManuallyDrop::new(Some(sample));
                }
                let mut status = 0u32;
                let processed = self.transform.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
                let sample = std::mem::ManuallyDrop::take(&mut output.pSample);
                let _events = std::mem::ManuallyDrop::take(&mut output.pEvents);
                match processed {
                    Ok(()) => {
                        let sample = sample.ok_or_else(|| {
                            MediaError::message("encoder produced an empty sample")
                        })?;
                        let bytes = sample_bytes(&sample)?;
                        if let Some(frame) = self.packetize(&bytes) {
                            frames.push(frame);
                        }
                        // An async MFT allows one ProcessOutput per HaveOutput event.
                        if self.asynchronous {
                            break;
                        }
                    }
                    Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        format_changes += 1;
                        if format_changes > 4 {
                            return Err(MediaError::message(format!(
                                "encoder kept changing its output format ({err})"
                            )));
                        }
                        // Hardware encoders report this once, after the first
                        // output, so the caller can accept the real type
                        // (including the SPS/PPS sequence header).
                        self.accept_stream_change()?;
                        // The next picture arrives as another METransformHaveOutput.
                        // Calling ProcessOutput again here returns E_UNEXPECTED.
                        if self.asynchronous {
                            break;
                        }
                    }
                    Err(err) if err.code() == MF_E_BUFFERTOOSMALL && !self.provides_samples => {
                        let needed = self
                            .transform
                            .GetOutputStreamInfo(0)
                            .map(|info| info.cbSize as usize)
                            .unwrap_or(0);
                        let next = needed.max(self.out_buf.len().saturating_mul(2));
                        if next <= self.out_buf.len() || next > 16 * 1024 * 1024 {
                            return Err(MediaError::message(format!(
                                "encoder output buffer of {} bytes is still too small ({err})",
                                self.out_buf.len()
                            )));
                        }
                        self.out_buf.resize(next, 0);
                    }
                    Err(_) if self.asynchronous && !frames.is_empty() => break,
                    Err(err) => {
                        return Err(MediaError::message(format!(
                            "process output (encoder_allocates_samples={}, align={}, buf={}): {err}",
                            self.provides_samples, self.alignment, self.out_buf.len()
                        )));
                    }
                }
            }
            Ok(frames)
        }
    }

    /// Quick Sync's first ProcessOutput returns `MF_E_TRANSFORM_STREAM_CHANGE`.
    /// The output type it offers then carries the real codec configuration.
    unsafe fn accept_stream_change(&mut self) -> Result<(), MediaError> {
        unsafe {
            let offered = match self.transform.GetOutputAvailableType(0, 0) {
                Ok(ty) => ty,
                Err(_) => self.transform.GetOutputCurrentType(0)?,
            };
            if self.transform.SetOutputType(0, &offered, 0).is_err() {
                let current = self.transform.GetOutputCurrentType(0)?;
                self.transform.SetOutputType(0, &current, 0)?;
                self.note_sequence_header(&current);
            } else {
                self.note_sequence_header(&offered);
            }
            let info = self.transform.GetOutputStreamInfo(0)?;
            self.provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let need = (info.cbSize as usize).max(self.out_buf.len());
            if need > self.out_buf.len() && need <= 16 * 1024 * 1024 {
                self.out_buf.resize(need, 0);
            }
            Ok(())
        }
    }

    fn note_sequence_header(&mut self, ty: &IMFMediaType) {
        let Ok(attrs): Result<IMFAttributes, _> = ty.cast() else {
            return;
        };
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        let ok = unsafe { attrs.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut ptr, &mut len) };
        if ok.is_err() || ptr.is_null() || len == 0 {
            return;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
        unsafe { CoTaskMemFree(Some(ptr as *const _)) };
        self.absorb_parameter_sets(&bytes);
    }

    fn absorb_parameter_sets(&mut self, bytes: &[u8]) {
        for nal in nal_units(bytes) {
            match nal_type(&nal) {
                7 => self.sps = nal,
                8 => self.pps = nal,
                _ => {}
            }
        }
        if self.avcc.is_none() && !self.sps.is_empty() && !self.pps.is_empty() {
            self.avcc = Some(avc_decoder_config(&self.sps, &self.pps));
        }
    }

    fn packetize(&mut self, bytes: &[u8]) -> Option<VideoFrame> {
        let nals = nal_units(bytes);
        let mut vcl = Vec::new();
        let mut keyframe = false;
        for nal in &nals {
            match nal_type(nal) {
                7 => self.sps = nal.to_vec(),
                8 => self.pps = nal.to_vec(),
                5 => {
                    keyframe = true;
                    vcl.push(nal.as_slice());
                }
                1 => vcl.push(nal.as_slice()),
                9 => {}
                _ => vcl.push(nal.as_slice()),
            }
        }
        if self.avcc.is_none() && !self.sps.is_empty() && !self.pps.is_empty() {
            self.avcc = Some(avc_decoder_config(&self.sps, &self.pps));
        }
        if vcl.is_empty() {
            return None;
        }
        Some(VideoFrame {
            keyframe,
            avcc: length_prefixed(&vcl),
        })
    }
}

/// Annex-B, AVCC length-prefixed, or one raw NAL. Hardware MFTs use all three.
fn nal_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    if bytes.starts_with(&[0, 0, 0, 1]) || bytes.starts_with(&[0, 0, 1]) {
        return annexb_nals(bytes).into_iter().map(|nal| nal.to_vec()).collect();
    }
    let avcc = split_avcc(bytes);
    if !avcc.is_empty() {
        return avcc;
    }
    let kind = bytes[0] & 0x1F;
    if bytes[0] & 0x80 == 0 && (1..=23).contains(&kind) {
        return vec![bytes.to_vec()];
    }
    Vec::new()
}

fn split_avcc(data: &[u8]) -> Vec<Vec<u8>> {
    if data.len() < 5 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut index = 0usize;
    while index + 4 <= data.len() {
        let size = u32::from_be_bytes([
            data[index],
            data[index + 1],
            data[index + 2],
            data[index + 3],
        ]) as usize;
        let start = index + 4;
        if size == 0 || start + size > data.len() {
            return Vec::new();
        }
        let nal = &data[start..start + size];
        if nal[0] & 0x80 != 0 {
            return Vec::new();
        }
        out.push(nal.to_vec());
        index = start + size;
    }
    if index != data.len() {
        return Vec::new();
    }
    out
}

impl AudioEncoder {
    pub fn open(sample_rate: u32, bitrate: u32) -> Result<Self, MediaError> {
        unsafe {
            let (transform, name) = open_aac(sample_rate, bitrate)?;
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            Ok(Self {
                transform,
                sample_rate,
                pending: Vec::with_capacity(4096),
                name,
            })
        }
    }

    /// Push interleaved stereo s16 and return any complete AAC access units.
    pub fn push(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>, MediaError> {
        self.pending.extend_from_slice(pcm);
        let mut frames = Vec::new();
        while self.pending.len() >= 2048 {
            let block: Vec<i16> = self.pending.drain(..2048).collect();
            frames.extend(unsafe { self.encode_block(&block)? });
        }
        Ok(frames)
    }

    unsafe fn encode_block(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>, MediaError> {
        unsafe {
            let bytes = std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2);
            let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let pts = 0i64;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(10_000_000 * 1024 / i64::from(self.sample_rate.max(1)))?;
            self.transform.ProcessInput(0, &sample, 0)?;
            let mut out = Vec::new();
            loop {
                let buffer = MFCreateMemoryBuffer(2048)?;
                let produced = MFCreateSample()?;
                produced.AddBuffer(&buffer)?;
                let mut output = MFT_OUTPUT_DATA_BUFFER {
                    pSample: std::mem::ManuallyDrop::new(Some(produced)),
                    ..Default::default()
                };
                let mut status = 0u32;
                let processed = self.transform.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
                let produced = std::mem::ManuallyDrop::take(&mut output.pSample);
                let _events = std::mem::ManuallyDrop::take(&mut output.pEvents);
                match processed {
                    Ok(()) => {
                        let produced = produced.ok_or_else(|| MediaError::message("AAC encoder produced an empty sample"))?;
                        let bytes = sample_bytes(&produced)?;
                        let raw = if bytes.len() > 7 && bytes[0] == 0xFF && bytes[1] & 0xF0 == 0xF0 {
                            bytes[7..].to_vec()
                        } else {
                            bytes
                        };
                        if !raw.is_empty() {
                            out.push(raw);
                        }
                    }
                    Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(err) => return Err(err.into()),
                }
            }
            Ok(out)
        }
    }
}

unsafe fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, MediaError> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock()?;
        Ok(bytes)
    }
}

unsafe fn open_h264(
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<(IMFTransform, String, bool, bool), MediaError> {
    unsafe {
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        for flags in [
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            MFT_ENUM_FLAG_HARDWARE,
        ] {
            if let Some(found) = activate_first(MFT_CATEGORY_VIDEO_ENCODER, flags, &output_info) {
                match configure_h264(&found.0, width, height, fps, bitrate) {
                    Ok((transform, asynchronous)) => return Ok((transform, found.1, true, asynchronous)),
                    Err(err) => eprintln!("hardware encoder {}: {err}", found.1),
                }
            }
        }
        let found = activate_first(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            &output_info,
        )
        .ok_or_else(|| MediaError::message("no H.264 encoder is installed"))?;
        let (transform, asynchronous) = configure_h264(&found.0, width, height, fps, bitrate)?;
        Ok((transform, found.1, false, asynchronous))
    }
}

unsafe fn configure_h264(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<(IMFTransform, bool), MediaError> {
    unsafe {
        let mut asynchronous = false;
        if let Ok(attrs) = transform.GetAttributes() {
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                asynchronous = true;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        let profiles = [
            Some(eAVEncH264VProfile_Main.0 as u32),
            Some(eAVEncH264VProfile_Base.0 as u32),
            Some(eAVEncH264VProfile_High.0 as u32),
            None,
        ];
        let mut last_err = MediaError::message("encoder rejected every H.264 profile");
        for profile in profiles {
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack_size(fps, 1))?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            if let Some(profile) = profile {
                output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            }
            if let Err(err) = transform.SetOutputType(0, &output, 0) {
                last_err = err.into();
                continue;
            }
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack_size(fps, 1))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_size(1, 1))?;
            let _ = input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width);
            if let Err(err) = transform.SetInputType(0, &input, 0) {
                last_err = err.into();
                continue;
            }
            let _ = size_of::<MFT_REGISTER_TYPE_INFO>();
            return Ok((transform.clone(), asynchronous));
        }
        Err(last_err)
    }
}

unsafe fn open_aac(sample_rate: u32, bitrate: u32) -> Result<(IMFTransform, String), MediaError> {
    unsafe {
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_AAC,
        };
        let (transform, name) = activate_first(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            &output_info,
        )
        .ok_or_else(|| MediaError::message("no AAC encoder is installed"))?;
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
        output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bitrate / 8)?;
        let _ = output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0);
        transform.SetOutputType(0, &output, 0)?;

        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
        input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
        input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sample_rate * 4)?;
        transform.SetInputType(0, &input, 0)?;
        Ok((transform, name))
    }
}

unsafe fn activate_first(
    category: windows::core::GUID,
    flags: MFT_ENUM_FLAG,
    output: &MFT_REGISTER_TYPE_INFO,
) -> Option<(IMFTransform, String)> {
    unsafe {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFTEnumEx(category, flags, None, Some(output), &mut activates, &mut count).is_err() || count == 0 {
            return None;
        }
        let list = std::slice::from_raw_parts(activates, count as usize);
        let mut chosen = None;
        for activate in list.iter().flatten() {
            let name = friendly_name(activate);
            if let Ok(transform) = activate.ActivateObject::<IMFTransform>() {
                chosen = Some((transform, name));
                break;
            }
        }
        CoTaskMemFree(Some(activates as *const _));
        chosen
    }
}

unsafe fn friendly_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut value = windows::core::PWSTR::null();
        let mut len = 0u32;
        if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len).is_ok() {
            let text = value.to_string().unwrap_or_else(|_| "encoder".into());
            CoTaskMemFree(Some(value.0 as *const _));
            text
        } else {
            "encoder".into()
        }
    }
}

fn pack_size(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}
