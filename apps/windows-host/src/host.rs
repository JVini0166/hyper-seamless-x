use protocol::{Frame, Hello, LaunchApp, Pairing, Request, Response, Result, WindowInfo};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::{
    fs,
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame as CaptureFrame,
    graphics_capture_api::InternalCaptureControl,
    settings::*,
    window::Window,
};

type Stream = StreamOwned<ServerConnection, TcpStream>;

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [command, dir] if command == "init" => initialize(Path::new(dir)),
        [command, addr, dir] if command == "serve" => serve(addr.parse()?, Path::new(dir)),
        _ => Err(
            "Uso: windows-host init <nova-pasta> | windows-host serve <IP:porta> <pasta>".into(),
        ),
    }
}

fn initialize(path: &Path) -> Result<()> {
    let (pairing, key) = protocol::new_identity()?;
    fs::create_dir(path)?;
    fs::write(path.join("host-key.der"), key)?;
    fs::write(path.join("pairing.json"), serde_json::to_vec(&pairing)?)?;
    println!(
        "Identidade criada em {}. Transfira apenas pairing.json ao Mac.",
        path.display()
    );
    Ok(())
}

fn serve(addr: SocketAddr, path: &Path) -> Result<()> {
    let pairing: Pairing = serde_json::from_slice(&fs::read(path.join("pairing.json"))?)?;
    let config = Arc::new(pairing.server_config(fs::read(path.join("host-key.der"))?)?);
    let listener = TcpListener::bind(addr)?;
    let active = Arc::new(AtomicUsize::new(0));
    println!("Host interativo ouvindo em {}", listener.local_addr()?);
    for socket in listener.incoming() {
        let socket = socket?;
        if active.load(Ordering::Relaxed) >= 12 {
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let active = active.clone();
        let config = config.clone();
        let pairing = pairing.clone();
        thread::spawn(move || {
            if let Err(error) = handle(socket, config, &pairing) {
                eprintln!("Conexão encerrada: {error}");
            }
            active.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

fn handle(socket: TcpStream, config: Arc<ServerConfig>, pairing: &Pairing) -> Result<()> {
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    socket.set_nodelay(true)?;
    let mut stream = StreamOwned::new(ServerConnection::new(config)?, socket);
    let hello: Hello = protocol::receive(&mut stream)?;
    hello.verify(pairing)?;
    match hello.request {
        Request::List => {
            let mut windows: Vec<_> = Window::enumerate()?
                .into_iter()
                .filter_map(|window| {
                    let title = window.title().ok()?;
                    if title.trim().is_empty() {
                        return None;
                    }
                    Some(WindowInfo {
                        handle: window.as_raw_hwnd() as u64,
                        process_id: window.process_id().ok()?,
                        title: title.chars().take(256).collect(),
                        process: window
                            .process_name()
                            .unwrap_or_default()
                            .chars()
                            .take(128)
                            .collect(),
                    })
                })
                .take(128)
                .collect();
            windows.sort_by_cached_key(|window| window.title.to_lowercase());
            protocol::send(&mut stream, &Response::Windows(windows))
        }
        Request::Launch(app) => {
            let root = std::env::var_os("SystemRoot").ok_or("SystemRoot indisponível")?;
            let name = match app {
                LaunchApp::Explorer => "explorer.exe",
                LaunchApp::Notepad => "System32/notepad.exe",
            };
            let response = match Command::new(Path::new(&root).join(name)).spawn() {
                Ok(_) => Response::Launched,
                Err(error) => Response::Error(error.to_string()),
            };
            protocol::send(&mut stream, &response)
        }
        Request::View { handle, process_id } => {
            let window = Window::enumerate()?.into_iter().find(|window| {
                window.as_raw_hwnd() as u64 == handle
                    && window.process_id().ok() == Some(process_id)
            });
            match window {
                Some(window) => capture(window, &mut stream),
                None => protocol::send(&mut stream, &Response::Error("Janela indisponível".into())),
            }
        }
    }
}

type Latest = Arc<Mutex<Option<Frame>>>;

struct Capture {
    latest: Latest,
    scratch: Vec<u8>,
    last: Instant,
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = Latest;
    type Error = protocol::Error;

    fn new(context: Context<Self::Flags>) -> Result<Self> {
        Ok(Self {
            latest: context.flags,
            scratch: Vec::new(),
            last: Instant::now() - Duration::from_secs(1),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut CaptureFrame,
        _: InternalCaptureControl,
    ) -> Result<()> {
        if self.last.elapsed() < Duration::from_millis(33) {
            return Ok(());
        }
        let (width, height) = (frame.width(), frame.height());
        if u64::from(width) * u64::from(height) > protocol::MAX_PIXELS as u64 {
            return Err("Resolução da janela excede o limite".into());
        }
        let bgra = frame
            .buffer()?
            .as_nopadding_buffer(&mut self.scratch)
            .to_vec();
        *self
            .latest
            .lock()
            .map_err(|_| "Fila de captura indisponível")? = Some(Frame {
            width,
            height,
            bgra,
        });
        self.last = Instant::now();
        Ok(())
    }
}

fn capture(window: Window, stream: &mut Stream) -> Result<()> {
    let latest = Latest::default();
    let settings = Settings::new(
        window,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        latest.clone(),
    );
    let control = match Capture::start_free_threaded(settings) {
        Ok(control) => control,
        Err(error) => return protocol::send(stream, &Response::Error(error.to_string())),
    };
    let result = (|| {
        protocol::send(stream, &Response::Streaming)?;
        let mut last_write = Instant::now();
        while !control.is_finished() {
            let frame = latest
                .lock()
                .map_err(|_| "Fila de captura indisponível")?
                .take();
            if let Some(frame) = frame {
                frame.send(stream)?;
                last_write = Instant::now();
            } else if last_write.elapsed() >= Duration::from_secs(2) {
                protocol::heartbeat(stream)?;
                last_write = Instant::now();
            } else {
                thread::sleep(Duration::from_millis(8));
            }
        }
        stream.conn.send_close_notify();
        stream.flush()?;
        Ok(())
    })();
    let stop = control.stop();
    result.and_then(|()| Ok(stop?))
}
