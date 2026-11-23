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
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
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
        Request::View {
            handle,
            process_id,
            metrics,
            h264,
            rtc_offer,
        } => match find_window(handle, process_id)? {
            Some(window) => {
                let rtc = stream
                    .sock
                    .lan_ip()
                    .zip(rtc_offer.as_deref())
                    .filter(|_| h264)
                    .and_then(|(ip, offer)| protocol::rtc::VideoPeer::answer(ip, offer).ok());
                capture(window, &mut stream, stop, metrics, h264, rtc)
            }
            None => protocol::send(&mut stream, &Response::Error("Janela indisponível".into())),
        },
        Request::Input { handle, process_id } => serve_input(&mut stream, handle, process_id, stop),
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

fn serve_input(stream: &mut Stream, handle: u64, process_id: u32, stop: &AtomicBool) -> Result<()> {
    use protocol::InputEvent;
    if find_window(handle, process_id)?.is_none() {
        return protocol::send(stream, &Response::Error("Janela indisponível".into()));
    }
    protocol::send(stream, &Response::InputReady)?;
    while !stop.load(Ordering::Acquire) {
        let event: InputEvent = protocol::receive(stream)?;
        if stop.load(Ordering::Acquire) {
            break;
        }
        let Some(window) = find_window(handle, process_id)? else {
            return protocol::send(stream, &Response::Error("Janela indisponível".into()));
        };
        let result = match event {
            InputEvent::Click { x, y, button } => click_window(window, x, y, button),
            InputEvent::Scroll {
                x,
                y,
                horizontal,
                vertical,
            } => scroll_window(window, x, y, horizontal, vertical),
            InputEvent::Key(key) => send_key(window, key),
        };
        let response = match result {
            Ok(()) => Response::Controlled,
            Err(error) => Response::Error(error.to_string()),
        };
        protocol::send(stream, &response)?;
    }
    Ok(())
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

#[derive(Default)]
struct CaptureQueue {
    newest: Mutex<Option<CapturedPacket>>,
    dropped: AtomicU32,
    force_keyframe: AtomicBool,
    interval_ms: AtomicU32,
    bitrate_limit: AtomicU32,
}

enum CapturedPacket {
    Raw(Frame, u32),
    H264(Vec<u8>),
}

type Latest = Arc<CaptureQueue>;

struct Capture {
    latest: Latest,
    scratch: Vec<u8>,
    last: Instant,
    gpu_enabled: bool,
    gpu_encoder: Option<crate::nvenc::Encoder>,
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    device_context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = (Latest, bool);
    type Error = protocol::Error;

    fn new(context: Context<Self::Flags>) -> Result<Self> {
        Ok(Self {
            latest: context.flags.0,
            gpu_enabled: context.flags.1,
            gpu_encoder: None,
            device: context.device,
            device_context: context.device_context,
            scratch: Vec::new(),
            last: Instant::now() - Duration::from_secs(1),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut CaptureFrame,
        _: InternalCaptureControl,
    ) -> Result<()> {
        let started = Instant::now();
        let interval_ms = self.latest.interval_ms.load(Ordering::Relaxed).max(33);
        if self.last.elapsed() < Duration::from_millis(u64::from(interval_ms)) {
            self.latest.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let (width, height) = (frame.width(), frame.height());
        if u64::from(width) * u64::from(height) > protocol::MAX_PIXELS as u64 {
            return Err("Resolução da janela excede o limite".into());
        }
        if self.gpu_enabled {
            let pending = self
                .latest
                .newest
                .lock()
                .map_err(|_| "Fila de captura indisponível")?
                .is_some();
            if pending {
                // Drop before encode so the wire never skips a reference picture.
                self.latest.dropped.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            match self.encode_gpu(frame, width, height) {
                Ok(unit) => {
                    *self
                        .latest
                        .newest
                        .lock()
                        .map_err(|_| "Fila de captura indisponível")? =
                        Some(CapturedPacket::H264(unit));
                    self.last = Instant::now();
                    return Ok(());
                }
                Err(_) => {
                    self.gpu_encoder = None;
                    self.gpu_enabled = false;
                }
            }
        }
        let bgra = frame
            .buffer()?
            .as_nopadding_buffer(&mut self.scratch)
            .to_vec();
        let elapsed = protocol::duration_micros(started.elapsed());
        let overwritten = self
            .latest
            .newest
            .lock()
            .map_err(|_| "Fila de captura indisponível")?
            .replace(CapturedPacket::Raw(
                Frame {
                    width,
                    height,
                    bgra,
                },
                elapsed,
            ));
        if overwritten.is_some() {
            self.latest.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.last = Instant::now();
        Ok(())
    }
}

impl Capture {
    fn encode_gpu(&mut self, frame: &CaptureFrame, width: u32, height: u32) -> Result<Vec<u8>> {
        let force_keyframe = self.latest.force_keyframe.swap(false, Ordering::AcqRel);
        let bitrate_limit = self.latest.bitrate_limit.load(Ordering::Relaxed);
        let first = !self
            .gpu_encoder
            .as_ref()
            .is_some_and(|encoder| encoder.matches(width, height, bitrate_limit));
        if first {
            self.gpu_encoder = None;
            self.gpu_encoder = Some(crate::nvenc::Encoder::from_capture_device(
                width,
                height,
                bitrate_limit,
                &self.device,
                &self.device_context,
            )?);
        }
        let unit = self
            .gpu_encoder
            .as_mut()
            .ok_or("Encoder NVENC indisponível")?
            .encode_capture_texture(frame.as_raw_texture(), force_keyframe)?;
        if first && !crate::h264::ready_to_decode(&unit) {
            return Err("Primeiro quadro NVENC sem parâmetros H.264".into());
        }
        Ok(unit)
    }
}

fn capture(
    window: Window,
    stream: &mut Stream,
    stop: &AtomicBool,
    metrics: bool,
    h264: bool,
    rtc: Option<(protocol::rtc::VideoPeer, String)>,
) -> Result<()> {
    let mut rtc = if let Some((mut peer, answer)) = rtc {
        protocol::send(stream, &Response::StreamingRtc { answer })?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !peer.ready() {
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Ok(());
            }
            peer.poll()?;
            thread::sleep(Duration::from_millis(2));
        }
        Some(peer)
    } else {
        None
    };
    let latest = Latest::default();
    let settings = Settings::new(
        window,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        (latest.clone(), h264),
    );
    let control = match Capture::start_free_threaded(settings) {
        Ok(control) => control,
        Err(error) => return protocol::send(stream, &Response::Error(error.to_string())),
    };
    let result = (|| {
        let mut announced = !h264 || rtc.is_some();
        let mut h264_enabled = h264;
        let mut last_encoder = None;
        let mut encoder: Option<crate::h264::Encoder> = None;
        if !h264 {
            protocol::send(stream, &Response::Streaming)?;
        }
        let mut last_write = Instant::now();
        let mut last_heartbeat = Instant::now();
        let mut last_keyframe_request = None;
        let mut last_bitrate_change = None;
        while !control.is_finished() && !stop.load(Ordering::Acquire) {
            if let Some(peer) = rtc.as_mut() {
                peer.poll()?;
                if let Some(budget) = peer.bitrate_budget() {
                    let bitrate = budget.clamp(500_000, 20_000_000);
                    let current = latest.bitrate_limit.load(Ordering::Relaxed);
                    let changed = current.abs_diff(bitrate) >= (current / 5).max(250_000);
                    let settled = last_bitrate_change
                        .is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(2));
                    if changed && settled {
                        latest.bitrate_limit.store(bitrate, Ordering::Relaxed);
                        last_bitrate_change = Some(Instant::now());
                    }
                }
                if let Some(interval_ms) = peer.capture_interval_ms() {
                    latest.interval_ms.store(interval_ms, Ordering::Relaxed);
                }
                if peer.take_keyframe_request()
                    && last_keyframe_request
                        .is_none_or(|last: Instant| last.elapsed() >= Duration::from_millis(500))
                {
                    latest.force_keyframe.store(true, Ordering::Release);
                    last_keyframe_request = Some(Instant::now());
                }
                if !peer.ready() {
                    break;
                }
                if last_heartbeat.elapsed() >= Duration::from_secs(2) {
                    protocol::heartbeat(stream)?;
                    last_heartbeat = Instant::now();
                }
            }
            let frame = latest
                .newest
                .lock()
                .map_err(|_| "Fila de captura indisponível")?
                .take();
            if let Some(CapturedPacket::H264(unit)) = frame {
                if let Some(peer) = rtc.as_mut() {
                    peer.send(unit)?;
                    continue;
                }
                if !announced {
                    protocol::send(stream, &Response::StreamingH264)?;
                    announced = true;
                }
                send_h264(
                    stream,
                    &unit,
                    protocol::EncoderMode::NvencGpuCopy,
                    metrics,
                    &mut last_encoder,
                )?;
                last_write = Instant::now();
                continue;
            }
            if let Some(CapturedPacket::Raw(frame, capture_micros)) = frame {
                let dropped = latest.dropped.swap(0, Ordering::Relaxed);
                if h264_enabled {
                    if latest.force_keyframe.swap(false, Ordering::AcqRel) {
                        encoder = None;
                    }
                    let bitrate_limit = latest.bitrate_limit.load(Ordering::Relaxed);
                    let encoded = encode_h264_frame(&mut encoder, &frame, bitrate_limit);
                    match encoded {
                        Ok((unit, mode)) => {
                            if let Some(peer) = rtc.as_mut() {
                                peer.send(unit)?;
                                continue;
                            }
                            if !announced {
                                protocol::send(stream, &Response::StreamingH264)?;
                                announced = true;
                            }
                            send_h264(stream, &unit, mode, metrics, &mut last_encoder)?;
                            last_write = Instant::now();
                            continue;
                        }
                        Err(_) => {
                            if rtc.is_some() {
                                break;
                            }
                            if announced {
                                protocol::send_h264_access_unit(
                                    stream,
                                    protocol::H264_SWITCH_TO_BGRA,
                                )?;
                            }
                            h264_enabled = false;
                            encoder = None;
                        }
                    }
                }
                if !announced {
                    protocol::send(stream, &Response::Streaming)?;
                    announced = true;
                }
                frame.send_with_metrics(stream, metrics.then_some((capture_micros, dropped)))?;
                last_write = Instant::now();
            } else if rtc.is_none() && announced && last_write.elapsed() >= Duration::from_secs(2) {
                protocol::heartbeat(stream)?;
                last_write = Instant::now();
            } else {
                thread::sleep(Duration::from_millis(if rtc.is_some() { 2 } else { 8 }));
            }
        }
        if (control.is_finished() || stop.load(Ordering::Acquire)) && announced {
            protocol::finish_video(stream)?;
        }
        stream.conn.send_close_notify();
        stream.flush()?;
        Ok(())
    })();
    let stop = control.stop();
    result.and_then(|()| Ok(stop?))
}

fn encode_h264_frame(
    encoder: &mut Option<crate::h264::Encoder>,
    frame: &Frame,
    bitrate_limit: u32,
) -> Result<(Vec<u8>, protocol::EncoderMode)> {
    if !encoder
        .as_ref()
        .is_some_and(|encoder| encoder.matches(frame, bitrate_limit))
    {
        // Release the previous COM session before initializing a new format.
        *encoder = None;
        *encoder = Some(crate::h264::Encoder::new(
            frame.width,
            frame.height,
            bitrate_limit,
        )?);
    }
    let encoder = encoder.as_mut().ok_or("Encoder indisponível")?;
    let packet = encoder.encode(frame)?;
    Ok((packet, encoder.mode()))
}

fn send_h264(
    stream: &mut Stream,
    unit: &[u8],
    mode: protocol::EncoderMode,
    metrics: bool,
    last: &mut Option<protocol::EncoderMode>,
) -> Result<()> {
    if metrics && *last != Some(mode) {
        protocol::send_h264_access_unit(stream, mode.marker())?;
        *last = Some(mode);
    }
    protocol::send_h264_access_unit(stream, unit)
}

fn click_window(window: Window, x: u16, y: u16, button: protocol::PointerButton) -> Result<()> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let flags = match button {
        protocol::PointerButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        protocol::PointerButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
    };
    pointer_window(window, x, y, &[(flags.0, 0), (flags.1, 0)], true)
}

fn scroll_window(window: Window, x: u16, y: u16, horizontal: i16, vertical: i16) -> Result<()> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let flags = [
        (MOUSEEVENTF_HWHEEL, horizontal),
        (MOUSEEVENTF_WHEEL, vertical),
    ];
    let events: Vec<_> = flags
        .into_iter()
        .filter(|(_, steps)| *steps != 0)
        .map(|(flag, steps)| (flag, i32::from(steps.clamp(-8, 8)) * 120))
        .collect();
    if events.is_empty() {
        return Ok(());
    }
    pointer_window(window, x, y, &events, false)
}

fn pointer_window(
    window: Window,
    x: u16,
    y: u16,
    events: &[(u32, i32)],
    click: bool,
) -> Result<()> {
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

    let _guard = INPUT_LOCK.lock().map_err(|_| "Entrada indisponível")?;
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
            return Err("O destino do ponteiro mudou".into());
        }
        let inputs: Vec<_> = events
            .iter()
            .map(|&(flags, data)| INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dwFlags: flags,
                        mouseData: data as u32,
                        ..Default::default()
                    },
                },
            })
            .collect();
        let sent = SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        );
        if sent != inputs.len() as u32 {
            if click && sent == 1 {
                SendInput(1, &inputs[1], size_of::<INPUT>() as i32);
            }
            return Err("O Windows recusou a entrada do ponteiro".into());
        }
    }
    Ok(())
}

static INPUT_LOCK: Mutex<()> = Mutex::new(());

fn send_key(window: Window, key: protocol::RemoteKey) -> Result<()> {
    use protocol::RemoteKey::*;
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::*,
        WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
    };

    let _guard = INPUT_LOCK.lock().map_err(|_| "Entrada indisponível")?;
    let hwnd = window.as_raw_hwnd();
    let expected_pid = window.process_id()?;
    let is_shortcut = matches!(key, EditingShortcut(_));
    let inputs = match key {
        Text(value) => {
            if value.is_empty() || value.len() > 32 || value.chars().any(char::is_control) {
                return Err("Texto de teclado inválido".into());
            }
            value
                .encode_utf16()
                .flat_map(|unit| keyboard_pair(0, unit, KEYEVENTF_UNICODE))
                .collect::<Vec<_>>()
        }
        EditingShortcut(shortcut) => {
            use protocol::EditingShortcut::*;
            let vk = match shortcut {
                SelectAll => VK_A,
                Copy => VK_C,
                Cut => VK_X,
                Paste => VK_V,
                Undo => VK_Z,
                Redo => VK_Y,
            };
            let [control_down, control_up] = keyboard_pair(VK_CONTROL, 0, 0);
            let [key_down, key_up] = keyboard_pair(vk, 0, 0);
            vec![control_down, key_down, key_up, control_up]
        }
        key => {
            let vk = match key {
                Backspace => VK_BACK,
                Delete => VK_DELETE,
                Enter => VK_RETURN,
                Tab => VK_TAB,
                Escape => VK_ESCAPE,
                Left => VK_LEFT,
                Right => VK_RIGHT,
                Up => VK_UP,
                Down => VK_DOWN,
                Home => VK_HOME,
                End => VK_END,
                PageUp => VK_PRIOR,
                PageDown => VK_NEXT,
                Text(_) | EditingShortcut(_) => unreachable!(),
            };
            keyboard_pair(vk, 0, 0).to_vec()
        }
    };
    // Keyboard events go to the foreground input queue. Refuse a different target.
    unsafe {
        let mut current_pid = 0;
        if GetForegroundWindow() != hwnd
            || GetWindowThreadProcessId(hwnd, &mut current_pid) == 0
            || current_pid != expected_pid
        {
            return Err("Traga a janela para frente no Windows antes de digitar".into());
        }
        let sent = SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        ) as usize;
        if sent != inputs.len() {
            if is_shortcut {
                if sent == 2 {
                    SendInput(2, inputs[2..].as_ptr(), size_of::<INPUT>() as i32);
                } else if sent != 0 {
                    SendInput(1, &inputs[3], size_of::<INPUT>() as i32);
                }
            } else if sent % 2 == 1 {
                SendInput(1, &inputs[sent], size_of::<INPUT>() as i32);
            }
            return Err("O Windows recusou o evento de teclado".into());
        }
    }
    Ok(())
}

fn keyboard_pair(
    vk: u16,
    scan: u16,
    flags: u32,
) -> [windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT; 2] {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    [flags, flags | KEYEVENTF_KEYUP].map(|dwFlags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags,
                ..Default::default()
            },
        },
    })
}
