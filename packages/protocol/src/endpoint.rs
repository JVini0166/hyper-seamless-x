use crate::{DEFAULT_PORT, Result};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    io::{self, Cursor, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, protocol::WebSocketConfig};
use url::Url;

const WS_PATH: &str = "/hyper-seamless-x";
const WS_CHUNK: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Endpoint(Url);

impl Endpoint {
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() || input.len() > 1024 {
            return Err("Digite um endereço válido".into());
        }
        let input = if let Ok(addr) = input.parse::<SocketAddr>() {
            format!("tcp://{addr}")
        } else if let Ok(ip) = input.parse::<IpAddr>() {
            format!("tcp://{}", SocketAddr::new(ip, DEFAULT_PORT))
        } else if input.contains("://") {
            input.to_owned()
        } else {
            format!("tcp://{input}")
        };
        let mut url = Url::parse(&input)?;
        let scheme = match url.scheme() {
            "hx" | "tcp" => "tcp",
            "http" | "ws" => "ws",
            "https" | "wss" => "wss",
            _ => return Err("Use um IP, nome de host, tcp://, http:// ou https://".into()),
        };
        // The URL parser cannot switch between special and non-special schemes.
        if scheme != url.scheme() {
            url = Url::parse(&format!(
                "{scheme}{}",
                &input[input.find(':').unwrap_or(0)..]
            ))?;
        }
        if url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port() == Some(0)
        {
            return Err("Endereço inválido. Use o host e a porta do túnel".into());
        }
        if scheme == "tcp" {
            if !matches!(url.path(), "" | "/") {
                return Err("Endereços TCP não têm caminho".into());
            }
            if url.port().is_none() {
                url.set_port(Some(DEFAULT_PORT))
                    .map_err(|_| "Porta inválida")?;
            }
            url.set_path("");
        } else if matches!(url.path(), "" | "/") {
            url.set_path(WS_PATH);
        } else if url.path() != WS_PATH {
            return Err("Use a URL base do túnel".into());
        }
        Ok(Self(url))
    }

    pub fn connect(&self, read_timeout: Duration) -> Result<Wire> {
        let host = match self.0.host().ok_or("Endereço sem host")? {
            url::Host::Domain(name) => name.to_owned(),
            url::Host::Ipv4(ip) => ip.to_string(),
            url::Host::Ipv6(ip) => ip.to_string(),
        };
        let port = self.0.port_or_known_default().ok_or("Endereço sem porta")?;
        let addresses = (host.as_str(), port).to_socket_addrs()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last_error = None;
        for address in addresses {
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                break;
            }
            match TcpStream::connect_timeout(&address, timeout.min(Duration::from_secs(2))) {
                Ok(socket) => {
                    socket.set_read_timeout(Some(read_timeout))?;
                    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
                    socket.set_nodelay(true)?;
                    if self.0.scheme() == "tcp" {
                        return Ok(Wire(Box::new(socket)));
                    }
                    let request = self.0.as_str().into_client_request()?;
                    let (ws, _) = tungstenite::client_tls_with_config(
                        request,
                        socket,
                        Some(ws_config()),
                        None,
                    )
                    .map_err(|error| match error {
                        tungstenite::HandshakeError::Failure(error) => websocket_error(error),
                        error => io::Error::other(error.to_string()),
                    })?;
                    return Ok(Wire(Box::new(SocketWire::new(ws))));
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::other("Nenhum endereço encontrado"))
            .into())
    }
}

impl TryFrom<String> for Endpoint {
    type Error = crate::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<Endpoint> for String {
    fn from(value: Endpoint) -> Self {
        value.0.into()
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.scheme() {
            "tcp" => formatter.write_str(
                self.0
                    .as_str()
                    .trim_start_matches("tcp://")
                    .trim_end_matches('/'),
            ),
            _ => formatter.write_str(self.0.as_str()),
        }
    }
}

trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send> Duplex for T {}

pub struct Wire(Box<dyn Duplex>);

impl Read for Wire {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Wire {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub fn use_incoming(socket: TcpStream) -> Result<Wire> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    socket.set_nodelay(true)?;
    let mut first = [0];
    socket.peek(&mut first)?;
    match first[0] {
        0x16 => Ok(Wire(Box::new(socket))), // TLS handshake record.
        b'G' => {
            let ws = tungstenite::accept_hdr_with_config(
                socket,
                |request: &tungstenite::handshake::server::Request, response| {
                    if request.uri().path() == WS_PATH && request.uri().query().is_none() {
                        Ok(response)
                    } else {
                        Err(tungstenite::http::Response::builder()
                            .status(404)
                            .body(None)
                            .expect("valid response"))
                    }
                },
                Some(ws_config()),
            )
            .map_err(|error| io::Error::other(error.to_string()))?;
            Ok(Wire(Box::new(SocketWire::new(ws))))
        }
        _ => {
            let _ = socket.shutdown(Shutdown::Both);
            Err("Transporte inválido".into())
        }
    }
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(WS_CHUNK)
        .write_buffer_size(0)
        .max_message_size(Some(WS_CHUNK))
        .max_frame_size(Some(WS_CHUNK))
}

struct SocketWire<S> {
    ws: WebSocket<S>,
    pending: Cursor<Vec<u8>>,
}

impl<S> SocketWire<S> {
    fn new(ws: WebSocket<S>) -> Self {
        Self {
            ws,
            pending: Cursor::new(Vec::new()),
        }
    }
}

impl<S: Read + Write> Read for SocketWire<S> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let buffered = self.pending.read(buffer)?;
            if buffered != 0 {
                return Ok(buffered);
            }
            match self.ws.read() {
                Ok(Message::Binary(bytes)) => self.pending = Cursor::new(bytes.to_vec()),
                Ok(Message::Ping(_)) => self.ws.flush().map_err(websocket_error)?,
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => return Ok(0),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Expected binary WebSocket data",
                    ));
                }
                Err(error) => return Err(websocket_error(error)),
            }
        }
    }
}

impl<S: Read + Write> Write for SocketWire<S> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let len = buffer.len().min(WS_CHUNK);
        if len != 0 {
            self.ws
                .write(Message::Binary(buffer[..len].to_vec().into()))
                .map_err(websocket_error)?;
        }
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ws.flush().map_err(websocket_error)
    }
}

fn websocket_error(error: tungstenite::Error) -> io::Error {
    match error {
        tungstenite::Error::Io(error) => error,
        tungstenite::Error::ConnectionClosed
        | tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ) => io::Error::new(io::ErrorKind::UnexpectedEof, error.to_string()),
        tungstenite::Error::Http(ref response)
            if matches!(response.status().as_u16(), 502..=504) =>
        {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("O túnel retornou HTTP {}", response.status()),
            )
        }
        error => io::Error::other(error.to_string()),
    }
}
