//! Minimal RTMP/RTMPS publisher.
//!
//! Enough of the protocol to publish one H.264 + AAC stream to an X ingest
//! (`rtmps://…pscp.tv:443/x`). It is not a general RTMP stack.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use native_tls::TlsStream;

use crate::MediaError;

const RTMP_VERSION: u8 = 3;
const HANDSHAKE: usize = 1536;

#[derive(Clone, Debug)]
enum Amf {
    Number(f64),
    Bool(bool),
    String(String),
    Object(BTreeMap<String, Amf>),
    Null,
}

pub struct RtmpPublisher {
    stream: TlsStream<TcpStream>,
    read_buf: VecDeque<u8>,
    in_chunk: usize,
    out_chunk: usize,
    channels: HashMap<u32, ChunkIn>,
    bytes_in: u32,
    window: u32,
    stream_id: u32,
    app: String,
}

struct ChunkIn {
    timestamp: u32,
    delta: u32,
    length: usize,
    type_id: u8,
    stream_id: u32,
    got_header: bool,
    filled: usize,
    body: Vec<u8>,
}

struct Message {
    type_id: u8,
    body: Vec<u8>,
    timestamp: u32,
}

impl RtmpPublisher {
    pub fn connect(url: &str, stream_key: &str) -> Result<Self, MediaError> {
        let target = parse_ingest(url)?;
        let tcp = connect_tcp(&target.host, target.port)?;
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
        tcp.set_write_timeout(Some(Duration::from_secs(10))).ok();

        let stream = if target.tls {
            let connector = native_tls::TlsConnector::new()
                .map_err(|err| MediaError::message(format!("tls: {err}")))?;
            connector
                .connect(&target.host, tcp)
                .map_err(|err| MediaError::message(format!("tls handshake: {err}")))?
        } else {
            return Err(MediaError::message(
                "X ingest is RTMPS. Refusing a cleartext rtmp:// URL.",
            ));
        };

        let mut publisher = Self {
            stream,
            read_buf: VecDeque::new(),
            in_chunk: 128,
            out_chunk: 4096,
            channels: HashMap::new(),
            bytes_in: 0,
            window: 2_500_000,
            stream_id: 0,
            app: target.app,
        };
        publisher.handshake()?;
        publisher.write_chunk_size(4096)?;
        publisher.write_window_ack(2_500_000)?;
        publisher.write_command(
            3,
            0,
            0,
            &[
                Amf::String("connect".into()),
                Amf::Number(1.0),
                Amf::Object({
                    let mut object = BTreeMap::new();
                    object.insert("app".into(), Amf::String(publisher.app.clone()));
                    object.insert("tcUrl".into(), Amf::String(target.tc_url));
                    object.insert(
                        "flashVer".into(),
                        Amf::String("FMLE/3.0 (compatible; XBroadcaster/0.1)".into()),
                    );
                    object.insert("fpad".into(), Amf::Bool(false));
                    object.insert("capabilities".into(), Amf::Number(15.0));
                    object.insert("audioCodecs".into(), Amf::Number(3191.0));
                    object.insert("videoCodecs".into(), Amf::Number(128.0));
                    object.insert("videoFunction".into(), Amf::Number(1.0));
                    object.insert("objectEncoding".into(), Amf::Number(0.0));
                    object
                }),
            ],
        )?;

        let connected = publisher.pump_until(Duration::from_secs(8), |message| {
            command_strings(message).iter().any(|value| {
                value.contains("NetConnection.Connect.Success") || value == "_result"
            })
        })?;
        if !connected {
            return Err(MediaError::message("RTMP server did not accept connect"));
        }

        publisher.write_command(
            3,
            0,
            0,
            &[
                Amf::String("releaseStream".into()),
                Amf::Number(2.0),
                Amf::Null,
                Amf::String(stream_key.into()),
            ],
        )?;
        publisher.write_command(
            3,
            0,
            0,
            &[
                Amf::String("FCPublish".into()),
                Amf::Number(3.0),
                Amf::Null,
                Amf::String(stream_key.into()),
            ],
        )?;
        publisher.write_command(
            3,
            0,
            0,
            &[
                Amf::String("createStream".into()),
                Amf::Number(4.0),
                Amf::Null,
            ],
        )?;

        let mut created = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while created.is_none() && std::time::Instant::now() < deadline {
            if let Some(message) = publisher.read_message()? {
                publisher.note_message(&message);
                if let Some(id) = stream_id_from_create(&message) {
                    created = Some(id);
                }
            }
        }
        publisher.stream_id = created.ok_or_else(|| MediaError::message("createStream returned no id"))?;

        publisher.write_command(
            8,
            0,
            publisher.stream_id,
            &[
                Amf::String("publish".into()),
                Amf::Number(0.0),
                Amf::Null,
                Amf::String(stream_key.into()),
                Amf::String("live".into()),
            ],
        )?;

        let mut publish_code = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while publish_code.is_none() && std::time::Instant::now() < deadline {
            if let Some(message) = publisher.read_message()? {
                publisher.note_message(&message);
                publish_code = command_strings(&message)
                    .into_iter()
                    .find(|value| value.starts_with("NetStream."));
            }
        }
        match publish_code.as_deref() {
            Some("NetStream.Publish.Start") => {}
            Some(code) => {
                return Err(MediaError::message(format!(
                    "X rejected the stream key ({code}). In Live Studio, copy the stream key again."
                )));
            }
            None => {
                return Err(MediaError::message(
                    "X ingest did not accept the stream. Copy the RTMPS URL and stream key from Live Studio again.",
                ));
            }
        }

        publisher
            .stream
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(5)))
            .ok();
        Ok(publisher)
    }

    pub fn write_metadata(
        &mut self,
        width: u32,
        height: u32,
        fps: u32,
        video_kbps: u32,
        sample_rate: u32,
        audio_kbps: u32,
    ) -> Result<(), MediaError> {
        let mut object = BTreeMap::new();
        object.insert("duration".into(), Amf::Number(0.0));
        object.insert("width".into(), Amf::Number(width as f64));
        object.insert("height".into(), Amf::Number(height as f64));
        object.insert("videodatarate".into(), Amf::Number(video_kbps as f64));
        object.insert("framerate".into(), Amf::Number(fps as f64));
        object.insert("videocodecid".into(), Amf::Number(7.0));
        object.insert("audiodatarate".into(), Amf::Number(audio_kbps as f64));
        object.insert("audiosamplerate".into(), Amf::Number(sample_rate as f64));
        object.insert("audiosamplesize".into(), Amf::Number(16.0));
        object.insert("stereo".into(), Amf::Bool(true));
        object.insert("audiocodecid".into(), Amf::Number(10.0));
        object.insert(
            "encoder".into(),
            Amf::String("XBroadcaster".into()),
        );
        let payload = encode_values(&[
            Amf::String("@setDataFrame".into()),
            Amf::String("onMetaData".into()),
            Amf::Object(object),
        ]);
        self.write_raw(6, 0, 18, self.stream_id, &payload)
    }

    pub fn write_avc_header(&mut self, avcc: &[u8]) -> Result<(), MediaError> {
        let mut body = Vec::with_capacity(5 + avcc.len());
        body.extend_from_slice(&[0x17, 0x00, 0x00, 0x00, 0x00]);
        body.extend_from_slice(avcc);
        self.write_raw(6, 0, 9, self.stream_id, &body)
    }

    pub fn write_video(&mut self, pts_ms: u32, keyframe: bool, avcc_nals: &[u8]) -> Result<(), MediaError> {
        let mut body = Vec::with_capacity(5 + avcc_nals.len());
        body.push(if keyframe { 0x17 } else { 0x27 });
        body.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
        body.extend_from_slice(avcc_nals);
        self.write_raw(6, pts_ms, 9, self.stream_id, &body)
    }

    pub fn write_aac_header(&mut self, asc: &[u8]) -> Result<(), MediaError> {
        let mut body = vec![0xAF, 0x00];
        body.extend_from_slice(asc);
        self.write_raw(4, 0, 8, self.stream_id, &body)
    }

    pub fn write_aac(&mut self, pts_ms: u32, raw: &[u8]) -> Result<(), MediaError> {
        let mut body = vec![0xAF, 0x01];
        body.extend_from_slice(raw);
        self.write_raw(4, pts_ms, 8, self.stream_id, &body)
    }

    /// Pull server control messages so the window acknowledgement stays current.
    pub fn poll(&mut self) -> Result<(), MediaError> {
        while let Some(message) = self.read_message()? {
            self.note_message(&message);
        }
        Ok(())
    }

    fn handshake(&mut self) -> Result<(), MediaError> {
        let mut c1 = vec![0u8; HANDSHAKE];
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u32)
            .unwrap_or(0);
        c1[0..4].copy_from_slice(&millis.to_be_bytes());
        getrandom::getrandom(&mut c1[8..]).map_err(|err| MediaError::message(format!("random: {err}")))?;
        self.stream.write_all(&[RTMP_VERSION]).map_err(io_err)?;
        self.stream.write_all(&c1).map_err(io_err)?;
        self.stream.flush().map_err(io_err)?;

        let mut s0 = [0u8; 1];
        self.read_exact_into(&mut s0)?;
        if s0[0] != RTMP_VERSION {
            return Err(MediaError::message(format!(
                "RTMP version {} from server, expected 3",
                s0[0]
            )));
        }
        let mut s1 = vec![0u8; HANDSHAKE];
        self.read_exact_into(&mut s1)?;
        let mut s2 = vec![0u8; HANDSHAKE];
        self.read_exact_into(&mut s2)?;
        self.stream.write_all(&s1).map_err(io_err)?;
        self.stream.flush().map_err(io_err)?;
        Ok(())
    }

    fn pump_until(
        &mut self,
        budget: Duration,
        mut pred: impl FnMut(&Message) -> bool,
    ) -> Result<bool, MediaError> {
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if let Some(message) = self.read_message()? {
                let hit = pred(&message);
                self.note_message(&message);
                if hit {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn note_message(&mut self, message: &Message) {
        if message.type_id == 1 && message.body.len() >= 4 {
            let size = u32::from_be_bytes(message.body[0..4].try_into().unwrap());
            if size >= 1 {
                self.in_chunk = size as usize;
            }
        }
        if message.type_id == 5 && message.body.len() >= 4 {
            self.window = u32::from_be_bytes(message.body[0..4].try_into().unwrap()).max(1);
        }
        if message.type_id == 20 {
            let strings = command_strings(message);
            if strings.iter().any(|value| value == "onBWDone") {
                let _ = self.write_command(
                    3,
                    0,
                    0,
                    &[
                        Amf::String("_result".into()),
                        Amf::Number(0.0),
                        Amf::Null,
                        Amf::Number(0.0),
                    ],
                );
            }
        }
    }

    fn write_chunk_size(&mut self, size: u32) -> Result<(), MediaError> {
        self.out_chunk = size as usize;
        self.write_raw(2, 0, 1, 0, &size.to_be_bytes())
    }

    fn write_window_ack(&mut self, size: u32) -> Result<(), MediaError> {
        self.write_raw(2, 0, 5, 0, &size.to_be_bytes())
    }

    fn write_command(&mut self, cs: u8, ts: u32, stream: u32, values: &[Amf]) -> Result<(), MediaError> {
        self.write_raw(cs, ts, 20, stream, &encode_values(values))
    }

    fn write_raw(
        &mut self,
        cs_id: u8,
        timestamp: u32,
        type_id: u8,
        stream_id: u32,
        payload: &[u8],
    ) -> Result<(), MediaError> {
        let bytes = encode_rtmp_message(self.out_chunk, cs_id, timestamp, type_id, stream_id, payload);
        self.stream.write_all(&bytes).map_err(io_err)?;
        self.stream.flush().map_err(io_err)?;
        Ok(())
    }

    fn maybe_ack(&mut self) -> Result<(), MediaError> {
        if self.bytes_in >= self.window {
            let seq = self.bytes_in;
            self.bytes_in = 0;
            self.write_raw(2, 0, 3, 0, &seq.to_be_bytes())?;
        }
        Ok(())
    }

    fn read_message(&mut self) -> Result<Option<Message>, MediaError> {
        loop {
            if let Some(message) = self.pull_chunk()? {
                return Ok(Some(message));
            }
            let mut tmp = [0u8; 4096];
            match self.stream.read(&mut tmp) {
                Ok(0) => return Err(MediaError::message("RTMP connection closed")),
                Ok(n) => {
                    self.read_buf.extend(&tmp[..n]);
                    self.bytes_in = self.bytes_in.wrapping_add(n as u32);
                    self.maybe_ack()?;
                }
                Err(err) if err.kind() == std::io::ErrorKind::TimedOut || err.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(None);
                }
                Err(err) => return Err(io_err(err)),
            }
        }
    }

    fn pull_chunk(&mut self) -> Result<Option<Message>, MediaError> {
        if self.read_buf.is_empty() {
            return Ok(None);
        }
        let first = self.read_buf[0];
        let fmt = first >> 6;
        let cs_marker = u32::from(first & 0x3F);
        let header_extra: usize = match cs_marker {
            0 => 1,
            1 => 2,
            _ => 0,
        };
        if self.read_buf.len() < 1 + header_extra {
            return Ok(None);
        }
        let cs_id = match cs_marker {
            0 => u32::from(self.read_buf[1]) + 64,
            1 => u32::from(self.read_buf[1]) + (u32::from(self.read_buf[2]) << 8) + 64,
            id => id,
        };
        let continuing = self
            .channels
            .get(&cs_id)
            .map(|state| state.filled > 0 && state.filled < state.length)
            .unwrap_or(false);
        let fmt = if continuing { 3 } else { fmt };
        let basic = 1 + header_extra;
        let msg_header: usize = if continuing {
            0
        } else {
            match fmt {
                0 => 11,
                1 => 7,
                2 => 3,
                _ => 0,
            }
        };
        if self.read_buf.len() < basic + msg_header {
            return Ok(None);
        }
        let mut timestamp_field = 0u32;
        if !continuing && fmt <= 2 {
            timestamp_field = (u32::from(self.read_buf[basic]) << 16)
                | (u32::from(self.read_buf[basic + 1]) << 8)
                | u32::from(self.read_buf[basic + 2]);
        }
        let extended = !continuing && timestamp_field == 0x00FF_FFFF;
        if self.read_buf.len() < basic + msg_header + if extended { 4 } else { 0 } {
            return Ok(None);
        }
        if extended {
            let at = basic + msg_header;
            timestamp_field = u32::from_be_bytes([
                self.read_buf[at],
                self.read_buf[at + 1],
                self.read_buf[at + 2],
                self.read_buf[at + 3],
            ]);
        }
        let header_len = basic + msg_header + if extended { 4 } else { 0 };

        if !self.channels.contains_key(&cs_id) {
            self.channels.insert(
                cs_id,
                ChunkIn {
                    timestamp: 0,
                    delta: 0,
                    length: 0,
                    type_id: 0,
                    stream_id: 0,
                    got_header: false,
                    filled: 0,
                    body: Vec::new(),
                },
            );
        }
        let state = self.channels.get_mut(&cs_id).unwrap();
        if !continuing {
            if fmt == 0 {
                let h = basic;
                state.delta = timestamp_field;
                state.timestamp = timestamp_field;
                state.length = (usize::from(self.read_buf[h + 3]) << 16)
                    | (usize::from(self.read_buf[h + 4]) << 8)
                    | usize::from(self.read_buf[h + 5]);
                state.type_id = self.read_buf[h + 6];
                state.stream_id = u32::from_le_bytes([
                    self.read_buf[h + 7],
                    self.read_buf[h + 8],
                    self.read_buf[h + 9],
                    self.read_buf[h + 10],
                ]);
            } else if fmt == 1 {
                let h = basic;
                state.delta = timestamp_field;
                state.timestamp = state.timestamp.wrapping_add(timestamp_field);
                state.length = (usize::from(self.read_buf[h + 3]) << 16)
                    | (usize::from(self.read_buf[h + 4]) << 8)
                    | usize::from(self.read_buf[h + 5]);
                state.type_id = self.read_buf[h + 6];
            } else if fmt == 2 {
                state.delta = timestamp_field;
                state.timestamp = state.timestamp.wrapping_add(timestamp_field);
            } else if state.got_header {
                state.timestamp = state.timestamp.wrapping_add(state.delta);
            } else {
                return Err(MediaError::message("RTMP continuation before a header"));
            }
            state.filled = 0;
            state.body.resize(state.length, 0);
            state.got_header = true;
        }

        let take = state.length.saturating_sub(state.filled).min(self.in_chunk);
        if self.read_buf.len() < header_len + take {
            return Ok(None);
        }
        for _ in 0..header_len {
            self.read_buf.pop_front();
        }
        for offset in 0..take {
            let byte = self.read_buf.pop_front().unwrap();
            let at = state.filled + offset;
            if at < state.body.len() {
                state.body[at] = byte;
            }
        }
        state.filled += take;
        if state.filled < state.length {
            return Ok(None);
        }
        let message = Message {
            type_id: state.type_id,
            body: state.body.clone(),
            timestamp: state.timestamp,
        };
        state.filled = state.length;
        // A completed message stays complete until the next header resets `filled`.
        state.filled = 0;
        state.body.clear();
        Ok(Some(message))
    }

    fn read_exact_into(&mut self, dest: &mut [u8]) -> Result<(), MediaError> {
        let mut filled = 0;
        while filled < dest.len() {
            if self.read_buf.is_empty() {
                let mut tmp = [0u8; 4096];
                let n = self.stream.read(&mut tmp).map_err(io_err)?;
                if n == 0 {
                    return Err(MediaError::message("RTMP handshake ended early"));
                }
                self.read_buf.extend(&tmp[..n]);
            }
            dest[filled] = self.read_buf.pop_front().unwrap();
            filled += 1;
        }
        Ok(())
    }
}

struct Ingest {
    tls: bool,
    host: String,
    port: u16,
    app: String,
    tc_url: String,
}

fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, MediaError> {
    let mut last = String::new();
    for attempt in 0..3 {
        match TcpStream::connect((host, port)) {
            Ok(stream) => return Ok(stream),
            Err(err) if err.raw_os_error() == Some(10013) && attempt < 2 => {
                last = err.to_string();
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(err) => {
                return Err(MediaError::message(format!("connect {host}:{port}: {err}")));
            }
        }
    }
    Err(MediaError::message(format!("connect {host}:{port}: {last}")))
}

fn parse_ingest(url: &str) -> Result<Ingest, MediaError> {
    let tls = if url.starts_with("rtmps://") {
        true
    } else if url.starts_with("rtmp://") {
        false
    } else {
        return Err(MediaError::message(format!("not an rtmp url: {url}")));
    };
    let rest = url.split_once("://").unwrap().1;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        let port = port
            .parse::<u16>()
            .map_err(|_| MediaError::message(format!("bad port in {url}")))?;
        (host.to_string(), port)
    } else {
        (authority.to_string(), if tls { 443 } else { 1935 })
    };
    if host.is_empty() {
        return Err(MediaError::message("missing ingest host"));
    }
    let app = path.trim_matches('/').to_string();
    if app.is_empty() {
        return Err(MediaError::message("missing RTMP app path"));
    }
    Ok(Ingest {
        tls,
        host,
        port,
        app,
        tc_url: url.trim_end_matches('/').to_string(),
    })
}

fn encode_values(values: &[Amf]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        encode_amf(&mut out, value);
    }
    out
}

fn encode_amf(out: &mut Vec<u8>, value: &Amf) {
    match value {
        Amf::Number(number) => {
            out.push(0x00);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Amf::Bool(value) => {
            out.push(0x01);
            out.push(u8::from(*value));
        }
        Amf::String(value) => {
            out.push(0x02);
            let bytes = value.as_bytes();
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        Amf::Object(fields) => {
            out.push(0x03);
            for (key, value) in fields {
                let bytes = key.as_bytes();
                out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
                out.extend_from_slice(bytes);
                encode_amf(out, value);
            }
            out.extend_from_slice(&[0x00, 0x00, 0x09]);
        }
        Amf::Null => out.push(0x05),
    }
}

fn command_strings(message: &Message) -> Vec<String> {
    if message.type_id != 20 && message.type_id != 18 {
        return Vec::new();
    }
    decode_amf_strings(&message.body)
}

fn decode_amf_strings(data: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = data;
    while !rest.is_empty() && take_amf(&mut rest, &mut found).is_ok() {}
    found
}

fn take_amf(data: &mut &[u8], found: &mut Vec<String>) -> Result<(), ()> {
    if data.is_empty() {
        return Err(());
    }
    let marker = data[0];
    *data = &data[1..];
    match marker {
        0x00 => skip(data, 8),
        0x01 => skip(data, 1),
        0x02 => read_counted_string(data, found, 2),
        0x03 => read_object(data, found),
        0x05 | 0x06 => Ok(()),
        0x08 => {
            skip(data, 4)?;
            read_object(data, found)
        }
        0x0A => {
            if data.len() < 4 {
                return Err(());
            }
            let count = u32::from_be_bytes(data[..4].try_into().unwrap());
            *data = &data[4..];
            for _ in 0..count {
                take_amf(data, found)?;
            }
            Ok(())
        }
        0x0B => skip(data, 10),
        0x0C => read_counted_string(data, found, 4),
        _ => Err(()),
    }
}

fn skip(data: &mut &[u8], len: usize) -> Result<(), ()> {
    if data.len() < len {
        return Err(());
    }
    *data = &data[len..];
    Ok(())
}

fn read_counted_string(data: &mut &[u8], found: &mut Vec<String>, len_bytes: usize) -> Result<(), ()> {
    if data.len() < len_bytes {
        return Err(());
    }
    let len = if len_bytes == 2 {
        usize::from(u16::from_be_bytes([data[0], data[1]]))
    } else {
        u32::from_be_bytes(data[..4].try_into().unwrap()) as usize
    };
    *data = &data[len_bytes..];
    if data.len() < len {
        return Err(());
    }
    found.push(String::from_utf8_lossy(&data[..len]).into_owned());
    *data = &data[len..];
    Ok(())
}

/// AMF0 object properties are a raw length-prefixed name followed by a typed value.
fn read_object(data: &mut &[u8], found: &mut Vec<String>) -> Result<(), ()> {
    loop {
        if data.len() < 3 {
            return Err(());
        }
        let key_len = usize::from(u16::from_be_bytes([data[0], data[1]]));
        if key_len == 0 {
            if data[2] != 0x09 {
                return Err(());
            }
            *data = &data[3..];
            return Ok(());
        }
        if data.len() < 2 + key_len {
            return Err(());
        }
        *data = &data[2 + key_len..];
        take_amf(data, found)?;
    }
}

fn stream_id_from_create(message: &Message) -> Option<u32> {
    if message.type_id != 20 {
        return None;
    }
    let strings = decode_amf_strings(&message.body);
    if !strings.iter().any(|value| value == "_result") {
        return None;
    }
    // Layout: string, number (transaction), null, number (id).
    let data = &message.body;
    if data.first().copied() != Some(0x02) || data.len() < 3 {
        return None;
    }
    let name_len = u16::from_be_bytes([data[1], data[2]]) as usize;
    let mut cursor = 3 + name_len;
    if data.get(cursor).copied() != Some(0x00) || data.len() < cursor + 9 {
        return None;
    }
    let transaction = f64::from_be_bytes(data[cursor + 1..cursor + 9].try_into().ok()?);
    if (transaction - 4.0).abs() > 0.1 {
        return None;
    }
    cursor += 9;
    if data.get(cursor).copied() == Some(0x05) {
        cursor += 1;
    }
    if data.get(cursor).copied() != Some(0x00) || data.len() < cursor + 9 {
        return None;
    }
    let id = f64::from_be_bytes(data[cursor + 1..cursor + 9].try_into().ok()?);
    if id >= 1.0 {
        Some(id as u32)
    } else {
        None
    }
}

pub fn encode_rtmp_message(
    chunk_size: usize,
    cs_id: u8,
    timestamp: u32,
    type_id: u8,
    stream_id: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 16);
    let stamp = timestamp.min(0x00FF_FFFE);
    let mut header = [0u8; 12];
    header[0] = cs_id & 0x3F;
    header[1..4].copy_from_slice(&[(stamp >> 16) as u8, (stamp >> 8) as u8, stamp as u8]);
    let len = payload.len();
    header[4] = (len >> 16) as u8;
    header[5] = (len >> 8) as u8;
    header[6] = len as u8;
    header[7] = type_id;
    header[8..12].copy_from_slice(&stream_id.to_le_bytes());
    out.extend_from_slice(&header);
    let first = payload.len().min(chunk_size);
    out.extend_from_slice(&payload[..first]);
    let mut offset = first;
    while offset < payload.len() {
        out.push(0xC0 | (cs_id & 0x3F));
        let end = (offset + chunk_size).min(payload.len());
        out.extend_from_slice(&payload[offset..end]);
        offset = end;
    }
    out
}

/// Build an AVCDecoderConfigurationRecord from SPS and PPS NALs, without start codes.
pub fn avc_decoder_config(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let profile = sps.first().copied().unwrap_or(0x42);
    let compat = sps.get(1).copied().unwrap_or(0);
    let level = sps.get(2).copied().unwrap_or(0x1E);
    let mut out = vec![
        0x01,
        profile,
        compat,
        level,
        0xFF,
        0xE1,
        (sps.len() >> 8) as u8,
        sps.len() as u8,
    ];
    out.extend_from_slice(sps);
    out.push(0x01);
    out.extend_from_slice(&[(pps.len() >> 8) as u8, pps.len() as u8]);
    out.extend_from_slice(pps);
    out
}

/// Split an Annex-B byte stream into NAL payloads, start codes removed.
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 < data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
            continue;
        }
        if data[index] == 0
            && data[index + 1] == 0
            && data[index + 2] == 0
            && data[index + 3] == 1
        {
            starts.push(index + 4);
            index += 4;
            continue;
        }
        index += 1;
    }
    if starts.is_empty() {
        return if data.is_empty() { Vec::new() } else { vec![data] };
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (ordinal, start) in starts.iter().copied().enumerate() {
        let end = if ordinal + 1 < starts.len() {
            let next = starts[ordinal + 1];
            let mut end = next;
            if end >= 3 && data[end - 3] == 0 && data[end - 2] == 0 && data[end - 1] == 1 {
                end -= 3;
                if end > start && data[end - 1] == 0 {
                    end -= 1;
                }
            }
            end
        } else {
            data.len()
        };
        if end > start {
            nals.push(&data[start..end]);
        }
    }
    nals
}

pub fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().copied().unwrap_or(0) & 0x1F
}

fn io_err(err: std::io::Error) -> MediaError {
    MediaError::message(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amf_string_round_trip_scan() {
        let bytes = encode_values(&[
            Amf::String("connect".into()),
            Amf::Number(1.0),
            Amf::Null,
        ]);
        assert_eq!(decode_amf_strings(&bytes), vec!["connect".to_string()]);
    }

    #[test]
    fn chunks_split_on_the_configured_boundary() {
        let payload = vec![7u8; 200];
        let encoded = encode_rtmp_message(128, 6, 1000, 9, 1, &payload);
        assert_eq!(encoded[0] & 0x3F, 6);
        assert_eq!(encoded[12 + 128], 0xC0 | 6);
        assert_eq!(encoded.len(), 12 + 200 + 1);
    }

    #[test]
    fn avcc_wraps_sps_and_pps() {
        let sps = [0x67, 0x42, 0xC0, 0x1E];
        let pps = [0x68, 0xCE, 0x38, 0x80];
        let avcc = avc_decoder_config(&sps, &pps);
        assert_eq!(avcc[0], 1);
        assert_eq!(avcc[1], 0x67);
        assert_eq!(&avcc[6..8], &[(sps.len() >> 8) as u8, sps.len() as u8]);
    }

    #[test]
    fn annex_b_splits_two_nals() {
        let data = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xCE];
        let nals = annexb_nals(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x67, 0x42]);
        assert_eq!(nals[1], &[0x68, 0xCE]);
    }

    #[test]
    fn on_status_code_is_inside_the_object() {
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&8u16.to_be_bytes());
        body.extend_from_slice(b"onStatus");
        body.push(0x00);
        body.extend_from_slice(&0f64.to_be_bytes());
        body.push(0x05);
        body.push(0x03);
        body.extend_from_slice(&4u16.to_be_bytes());
        body.extend_from_slice(b"code");
        body.push(0x02);
        let code = b"NetStream.Publish.Start";
        body.extend_from_slice(&(code.len() as u16).to_be_bytes());
        body.extend_from_slice(code);
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        let strings = decode_amf_strings(&body);
        assert!(strings.iter().any(|value| value == "NetStream.Publish.Start"));
    }

    #[test]
    fn parse_x_ingest_url() {
        let ingest = parse_ingest("rtmps://de.pscp.tv:443/x").unwrap();
        assert!(ingest.tls);
        assert_eq!(ingest.host, "de.pscp.tv");
        assert_eq!(ingest.port, 443);
        assert_eq!(ingest.app, "x");
    }
}
