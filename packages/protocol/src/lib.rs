use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub const VERSION: u16 = 1;
pub const SERVER_NAME: &str = "hyper-seamless-x.local";
pub const MAX_METADATA: usize = 256 * 1024;
pub const MAX_PIXELS: usize = 4096 * 2160;

#[derive(Clone, Serialize, Deserialize)]
pub struct Pairing {
    pub certificate: Vec<u8>,
    pub token: [u8; 32],
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
    List,
    View { handle: u64, process_id: u32 },
    Launch(LaunchApp),
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum LaunchApp {
    Explorer,
    Notepad,
}

#[derive(Serialize, Deserialize)]
pub enum Response {
    Windows(Vec<WindowInfo>),
    Streaming,
    Launched,
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

impl Frame {
    pub fn send(&self, writer: &mut impl Write) -> Result<()> {
        if pixel_bytes(self.width, self.height)? != self.bgra.len() {
            return Err("Buffer de vídeo inválido".into());
        }
        let mut data = Vec::new();
        data.extend_from_slice(&self.width.to_be_bytes());
        data.extend_from_slice(&self.height.to_be_bytes());
        zstd::stream::copy_encode(self.bgra.as_slice(), &mut data, 1)?;
        send_packet(writer, &data, MAX_FRAME)
    }

    pub fn receive(reader: &mut impl Read) -> Result<Option<Self>> {
        let data = receive_packet(reader, MAX_FRAME)?;
        if data.is_empty() {
            return Ok(None);
        }
        let (header, compressed) = data.split_first_chunk::<8>().ok_or("Frame incompleto")?;
        let width = u32::from_be_bytes(header[..4].try_into()?);
        let height = u32::from_be_bytes(header[4..].try_into()?);
        let size = pixel_bytes(width, height)?;
        let bgra = zstd::bulk::decompress(compressed, size)?;
        if bgra.len() != size {
            return Err("Buffer de vídeo inválido".into());
        }
        Ok(Some(Self {
            width,
            height,
            bgra,
        }))
    }
}

pub fn heartbeat(writer: &mut impl Write) -> Result<()> {
    send_packet(writer, &[], MAX_FRAME)
}
