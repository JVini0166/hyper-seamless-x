use protocol::{
    Frame, Hello, LaunchApp, Pairing, PairingGate, Request, Response, Result, VERSION,
    WindowAction, WindowInfo, Wire,
};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::{
    fs,
    io::{self, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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

type Stream = StreamOwned<ServerConnection, Wire>;

pub struct Server {
    pub pairing_gate: PairingGate,
    stop: Arc<AtomicBool>,
    listener: Option<thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.pairing_gate.close();
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

pub fn start(addr: SocketAddr, path: &Path) -> Result<Server> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let (pairing, key) = load_identity(path)?;
    let config = Arc::new(pairing.server_config(key)?);
    let stop = Arc::new(AtomicBool::new(false));
    let pairing_gate = PairingGate::default();
    let worker_stop = stop.clone();
    let gate = pairing_gate.clone();
    let listener = thread::spawn(move || {
        serve(listener, config, pairing, worker_stop, gate);
    });
    Ok(Server {
        pairing_gate,
        stop,
        listener: Some(listener),
    })
}

fn load_identity(path: &Path) -> Result<(Pairing, Vec<u8>)> {
    // Keep both identity parts in a single file so they cannot become mismatched.
    let path = path.join("host-identity.json");
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let identity = protocol::new_identity()?;
            use std::fs::OpenOptions;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            let written = serde_json::to_writer(&mut file, &identity)
                .map_err(protocol::Error::from)
                .and_then(|()| Ok(file.sync_all()?));
            drop(file);
            if let Err(error) = written {
                let _ = fs::remove_file(path);
                return Err(error);
            }
            Ok(identity)
        }
        Err(error) => Err(error.into()),
    }
}

fn serve(
    listener: TcpListener,
    config: Arc<ServerConfig>,
    pairing: Pairing,
    stop: Arc<AtomicBool>,
    gate: PairingGate,
) {
    let active = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Acquire) {
        let socket = match listener.accept() {
            Ok((socket, _)) => socket,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(40));
                continue;
            }
            Err(_) => break,
        };
        if active.load(Ordering::Relaxed) >= 12 {
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let (active, config, pairing, stop) = (
            active.clone(),
            config.clone(),
            pairing.clone(),
            stop.clone(),
        );
        let gate = gate.clone();
        thread::spawn(move || {
            let _ = handle(socket, config, &pairing, &stop, &gate);
            active.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

fn handle(
    socket: TcpStream,
    config: Arc<ServerConfig>,
    pairing: &Pairing,
    stop: &AtomicBool,
    gate: &PairingGate,
) -> Result<()> {
    let mut stream = StreamOwned::new(
        ServerConnection::new(config)?,
        protocol::use_incoming(socket)?,
    );
    let hello: Hello = protocol::receive(&mut stream)?;
    if stop.load(Ordering::Acquire) {
        return Err("Compartilhamento encerrado".into());
    }
    if hello.version != VERSION {
        return protocol::send(
            &mut stream,
            &Response::Error("Atualize os dois aplicativos para a mesma versão".into()),
        );
    }
    if matches!(hello.request, Request::Pair) {
        return protocol::accept_pairing(&mut stream, pairing, gate, || {
            !stop.load(Ordering::Acquire)
        });
    }
    hello.verify(pairing)?;
    match hello.request {
        Request::Pair => unreachable!("pairing handled above"),
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
        Request::View { handle, process_id } => match find_window(handle, process_id)? {
            Some(window) => capture(window, &mut stream, stop),
            None => protocol::send(&mut stream, &Response::Error("Janela indisponível".into())),
        },
        Request::Click {
            handle,
            process_id,
            x,
            y,
            button,
        } => {
            let result = find_window(handle, process_id)?
                .ok_or_else(|| protocol::Error::from("Janela indisponível"))
                .and_then(|window| click_window(window, x, y, button));
            let response = match result {
                Ok(()) => Response::Controlled,
                Err(error) => Response::Error(error.to_string()),
            };
            protocol::send(&mut stream, &response)
        }
        Request::Control {
            handle,
            process_id,
            action,
        } => {
            let response = match find_window(handle, process_id)? {
                Some(window) => match control_window(window, action) {
                    Ok(()) => Response::Controlled,
                    Err(error) => Response::Error(error.to_string()),
                },
                None => Response::Error("Janela indisponível".into()),
            };
            protocol::send(&mut stream, &response)
        }
    }
}

fn find_window(handle: u64, process_id: u32) -> Result<Option<Window>> {
    Ok(Window::enumerate()?.into_iter().find(|window| {
        window.as_raw_hwnd() as u64 == handle && window.process_id().ok() == Some(process_id)
    }))
}

fn control_window(window: Window, action: WindowAction) -> Result<()> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        PostMessageW, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, ShowWindowAsync, WM_CLOSE,
    };
    let hwnd = window.as_raw_hwnd();
    // Windows owns the handle. The caller verifies it against the current window list.
    let ok = unsafe {
        match action {
            WindowAction::Restore => ShowWindowAsync(hwnd, SW_RESTORE),
            WindowAction::Maximize => ShowWindowAsync(hwnd, SW_MAXIMIZE),
            WindowAction::Minimize => ShowWindowAsync(hwnd, SW_MINIMIZE),
            WindowAction::Close => PostMessageW(hwnd, WM_CLOSE, 0, 0),
        }
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
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

fn capture(window: Window, stream: &mut Stream, stop: &AtomicBool) -> Result<()> {
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
        while !control.is_finished() && !stop.load(Ordering::Acquire) {
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

fn click_window(window: Window, x: u16, y: u16, button: protocol::PointerButton) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{POINT, RECT},
        Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        UI::{
            Input::KeyboardAndMouse::*,
            WindowsAndMessaging::{
                GA_ROOT, GetAncestor, GetForegroundWindow, IsIconic, SetCursorPos,
                SetForegroundWindow, WindowFromPoint,
            },
        },
    };

    static POINTER: Mutex<()> = Mutex::new(());
    let _guard = POINTER.lock().map_err(|_| "Entrada indisponível")?;
    let hwnd = window.as_raw_hwnd();
    let mut rect = RECT::default();
    // The current HWND was matched by process ID and the host validates desktop hit testing.
    unsafe {
        if IsIconic(hwnd) != 0
            || DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut rect as *mut RECT).cast(),
                size_of::<RECT>() as u32,
            ) < 0
            || rect.right <= rect.left
            || rect.bottom <= rect.top
        {
            return Err("A janela precisa estar visível no Windows".into());
        }
        if GetForegroundWindow() != hwnd {
            SetForegroundWindow(hwnd);
        }
        let offset = |value, length| (i64::from(value) * i64::from(length - 1) / 65535) as i32;
        let point = POINT {
            x: rect.left + offset(x, rect.right - rect.left),
            y: rect.top + offset(y, rect.bottom - rect.top),
        };
        if GetForegroundWindow() != hwnd || GetAncestor(WindowFromPoint(point), GA_ROOT) != hwnd {
            return Err("Traga a janela para frente no Windows".into());
        }
        if SetCursorPos(point.x, point.y) == 0
            || GetForegroundWindow() != hwnd
            || GetAncestor(WindowFromPoint(point), GA_ROOT) != hwnd
        {
            return Err("O destino do clique mudou".into());
        }
        let (down, up) = match button {
            protocol::PointerButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            protocol::PointerButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        };
        let inputs = [down, up].map(|flags| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        });
        let sent = SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        );
        if sent != inputs.len() as u32 {
            if sent == 1 {
                SendInput(1, &inputs[1], size_of::<INPUT>() as i32);
            }
            return Err("O Windows recusou o clique".into());
        }
    }
    Ok(())
}
