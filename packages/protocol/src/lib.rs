mod endpoint;
mod pairing;
pub mod rtc;

pub use endpoint::{Endpoint, Wire, use_incoming};
pub use pairing::{PairingGate, PairingState, accept_pairing, request_pairing};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub const DISCOVERY_SERVICE: &str = "_hyper-seamless._tcp.local.";
pub const DEFAULT_PORT: u16 = 47651;
pub const VERSION: u16 = 2;
pub const SERVER_NAME: &str = "hyper-seamless-x.local";
pub const MAX_METADATA: usize = 256 * 1024;
pub const MAX_PIXELS: usize = 4096 * 2160;

#[derive(Clone, Serialize, Deserialize)]
pub struct Pairing {
    pub certificate: Vec<u8>,
    pub token: [u8; 32],
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub address: Endpoint,
    pub pairing: Pairing,
}

pub fn new_identity() -> Result<(Pairing, Vec<u8>)> {
    let certified = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()])?;
    let mut token = [0; 32];
    getrandom::fill(&mut token).map_err(|error| io::Error::other(error.to_string()))?;
    Ok((
        Pairing {
            certificate: certified.cert.der().to_vec(),
            token,
        },
        certified.signing_key.serialize_der(),
    ))
}

impl Pairing {
    pub fn client_config(&self) -> Result<rustls::ClientConfig> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.certificate.clone().into())?;
        Ok(rustls::ClientConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth())
    }

    pub fn server_config(&self, key: Vec<u8>) -> Result<rustls::ServerConfig> {
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        Ok(rustls::ServerConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![self.certificate.clone().into()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub handle: u64,
    pub process_id: u32,
    pub title: String,
    pub process: String,
}

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub token: [u8; 32],
    pub request: Request,
}

impl Hello {
    pub fn verify(&self, pairing: &Pairing) -> Result<()> {
        use subtle::ConstantTimeEq;
        if self.version != VERSION || !bool::from(self.token.ct_eq(&pairing.token)) {
            return Err("Versão incompatível ou pareamento recusado".into());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub enum Request {
    Pair,
    List,
    View {
        handle: u64,
        process_id: u32,
        #[serde(default)]
        metrics: bool,
        #[serde(default)]
        h264: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rtc_offer: Option<String>,
    },
    Launch(LaunchApp),
    Input {
        handle: u64,
        process_id: u32,
    },
    Control {
        handle: u64,
        process_id: u32,
        action: WindowAction,
    },
}

#[derive(Serialize, Deserialize)]
pub enum InputEvent {
    Click {
        x: u16,
        y: u16,
        button: PointerButton,
    },
    Scroll {
        x: u16,
        y: u16,
        horizontal: i16,
        vertical: i16,
    },
    Key(RemoteKey),
}

#[derive(Serialize, Deserialize)]
pub enum RemoteKey {
    Text(String),
    Backspace,
    Delete,
    Enter,
    Tab,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    EditingShortcut(EditingShortcut),
}

#[derive(Serialize, Deserialize)]
pub enum EditingShortcut {
    SelectAll,
    Copy,
    Cut,
    Paste,
    Undo,
    Redo,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum PointerButton {
    Left,
    Right,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum LaunchApp {
    Explorer,
    Notepad,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum WindowAction {
    Restore,
    Maximize,
    Minimize,
    Close,
}

#[derive(Serialize, Deserialize)]
pub enum Response {
    PairPending,
    Paired(Pairing),
    Windows(Vec<WindowInfo>),
    Streaming,
    StreamingH264,
    StreamingRtc { answer: String },
    InputReady,
    Launched,
    Controlled,
    Error(String),
}

pub fn send<T: Serialize>(writer: &mut impl Write, message: &T) -> Result<()> {
    let data = serde_json::to_vec(message)?;
    send_packet(writer, &data, MAX_METADATA)
}

pub fn receive<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    Ok(serde_json::from_slice(&receive_packet(
        reader,
        MAX_METADATA,
    )?)?)
}

fn send_packet(writer: &mut impl Write, data: &[u8], limit: usize) -> Result<()> {
    if data.len() > limit {
        return Err("Pacote excede o limite".into());
    }
    writer.write_all(&(data.len() as u32).to_be_bytes())?;
    writer.write_all(data)?;
    writer.flush()?;
    Ok(())
}

fn receive_packet(reader: &mut impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > limit {
        return Err("Pacote excede o limite".into());
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

const MAX_FRAME: usize = MAX_PIXELS * 4 + 256 * 1024;

fn pixel_bytes(width: u32, height: u32) -> Result<usize> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels == 0 || pixels > MAX_PIXELS as u64 {
        return Err("Dimensões de vídeo fora do limite".into());
    }
    Ok(pixels as usize * 4)
}

#[derive(Debug, Clone, Copy)]
pub struct HostFrameMetrics {
    pub capture_micros: u32,
    pub encode_micros: u32,
    pub dropped: u32,
}

pub struct ReceivedFrame {
    pub frame: Frame,
    pub host_metrics: Option<HostFrameMetrics>,
    pub compressed_bytes: usize,
    pub decode_micros: u32,
}

const MEASURED_FLAG: u32 = 1 << 31;

pub fn duration_micros(duration: std::time::Duration) -> u32 {
    duration.as_micros().min(u32::MAX.into()) as u32
}

impl Frame {
    pub fn send(&self, writer: &mut impl Write) -> Result<()> {
        self.send_with_metrics(writer, None)
    }

    pub fn send_with_metrics(
        &self,
        writer: &mut impl Write,
        capture: Option<(u32, u32)>,
    ) -> Result<()> {
        if pixel_bytes(self.width, self.height)? != self.bgra.len() {
            return Err("Buffer de vídeo inválido".into());
        }
        let mut data = Vec::new();
        let flags = if capture.is_some() { MEASURED_FLAG } else { 0 };
        data.extend_from_slice(&(self.width | flags).to_be_bytes());
        data.extend_from_slice(&self.height.to_be_bytes());
        if let Some((capture_micros, dropped)) = capture {
            data.extend_from_slice(&capture_micros.to_be_bytes());
            data.extend_from_slice(&0u32.to_be_bytes());
            data.extend_from_slice(&dropped.to_be_bytes());
            let encode_start = std::time::Instant::now();
            zstd::stream::copy_encode(self.bgra.as_slice(), &mut data, 1)?;
            data[12..16].copy_from_slice(&duration_micros(encode_start.elapsed()).to_be_bytes());
        } else {
            zstd::stream::copy_encode(self.bgra.as_slice(), &mut data, 1)?;
        }
        send_packet(writer, &data, MAX_FRAME)
    }

    pub fn receive(reader: &mut impl Read) -> Result<Option<Self>> {
        Ok(Self::receive_with_metrics(reader)?.map(|packet| packet.frame))
    }

    pub fn receive_with_metrics(reader: &mut impl Read) -> Result<Option<ReceivedFrame>> {
        let data = receive_packet(reader, MAX_FRAME)?;
        if data.is_empty() {
            return Ok(None);
        }
        let (header, payload) = data.split_first_chunk::<8>().ok_or("Frame incompleto")?;
        let tagged_width = u32::from_be_bytes(header[..4].try_into()?);
        let width = tagged_width & !MEASURED_FLAG;
        let height = u32::from_be_bytes(header[4..].try_into()?);
        let (host_metrics, compressed) = if tagged_width & MEASURED_FLAG != 0 {
            let (metrics, compressed) = payload
                .split_first_chunk::<12>()
                .ok_or("Métricas incompletas")?;
            (
                Some(HostFrameMetrics {
                    capture_micros: u32::from_be_bytes(metrics[..4].try_into()?),
                    encode_micros: u32::from_be_bytes(metrics[4..8].try_into()?),
                    dropped: u32::from_be_bytes(metrics[8..].try_into()?),
                }),
                compressed,
            )
        } else {
            (None, payload)
        };
        let size = pixel_bytes(width, height)?;
        let decode_start = std::time::Instant::now();
        let bgra = zstd::bulk::decompress(compressed, size)?;
        let decode_micros = duration_micros(decode_start.elapsed());
        if bgra.len() != size {
            return Err("Buffer de vídeo inválido".into());
        }
        Ok(Some(ReceivedFrame {
            frame: Self {
                width,
                height,
                bgra,
            },
            host_metrics,
            compressed_bytes: compressed.len(),
            decode_micros,
        }))
    }
}

const MAX_H264_ACCESS_UNIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderMode {
    NvencGpuCopy,
    NvencUpload,
    MediaFoundation,
}

impl EncoderMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::NvencGpuCopy => "nvenc-gpu-copy",
            Self::NvencUpload => "nvenc-upload",
            Self::MediaFoundation => "media-foundation",
        }
    }

    pub fn marker(self) -> &'static [u8] {
        match self {
            Self::NvencGpuCopy => b"HSX:ENCODER:NVENC-GPU",
            Self::NvencUpload => b"HSX:ENCODER:NVENC-UPLOAD",
            Self::MediaFoundation => b"HSX:ENCODER:MEDIA-FOUNDATION",
        }
    }

    pub fn from_marker(packet: &[u8]) -> Option<Self> {
        [Self::NvencGpuCopy, Self::NvencUpload, Self::MediaFoundation]
            .into_iter()
            .find(|mode| mode.marker() == packet)
    }
}

/// A negotiated H.264 stream can switch once to the BGRA fallback.
pub const H264_SWITCH_TO_BGRA: &[u8] = b"HSX:BGRA";

pub fn send_h264_access_unit(writer: &mut impl Write, annex_b: &[u8]) -> Result<()> {
    send_packet(writer, annex_b, MAX_H264_ACCESS_UNIT)
}

pub fn receive_h264_access_unit(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let packet = receive_packet(reader, MAX_H264_ACCESS_UNIT)?;
    Ok((!packet.is_empty()).then_some(packet))
}

pub fn heartbeat(writer: &mut impl Write) -> Result<()> {
    send_packet(writer, &[], MAX_FRAME)
}
