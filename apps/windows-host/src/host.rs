use protocol::{
    Frame, Hello, LaunchApp, Pairing, PairingGate, Request, Response, Result, VERSION,
    WindowAction, WindowInfo, Wire,
};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::{
    fs,
    io::{self, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
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
        Request::List | Request::ListCatalog => {
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
            let response = if matches!(hello.request, Request::ListCatalog) {
                let apps = [LaunchApp::Explorer, LaunchApp::Notepad, LaunchApp::VsCode]
                    .into_iter()
                    .filter(|app| launch_path(*app).is_ok_and(|path| path.is_file()))
                    .collect();
                Response::WindowCatalog { windows, apps }
            } else {
                Response::Windows(windows)
            };
            protocol::send(&mut stream, &response)
        }
        Request::Launch(app) => {
            let result = launch_path(app).and_then(|path| Ok(Command::new(path).spawn()?));
            let response = match result {
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
            h264_metrics,
        } => match find_window(handle, process_id)? {
            Some(window) => {
                let rtc = stream
                    .sock
                    .lan_ip()
                    .zip(rtc_offer.as_deref())
                    .filter(|_| h264)
                    .and_then(|(ip, offer)| protocol::rtc::VideoPeer::answer(ip, offer).ok());
                capture(window, &mut stream, stop, metrics, h264, h264_metrics, rtc)
            }
            None => protocol::send(&mut stream, &Response::Error("Janela indisponível".into())),
        },
        Request::Input {
            handle,
            process_id,
            focus,
            resize,
        } => serve_input(&mut stream, handle, process_id, focus, resize, stop),
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

fn launch_path(app: LaunchApp) -> Result<PathBuf> {
    if matches!(app, LaunchApp::VsCode) {
        return [
            ("LOCALAPPDATA", "Programs/Microsoft VS Code/Code.exe"),
            ("ProgramFiles", "Microsoft VS Code/Code.exe"),
            ("ProgramFiles(x86)", "Microsoft VS Code/Code.exe"),
        ]
        .into_iter()
        .filter_map(|(base, path)| {
            std::env::var_os(base).map(|base| PathBuf::from(base).join(path))
        })
        .find(|path| path.is_absolute() && path.is_file())
        .ok_or_else(|| "VS Code não encontrado nas pastas padrão do Windows".into());
    }
    let root = std::env::var_os("SystemRoot").ok_or("SystemRoot indisponível")?;
    let name = match app {
        LaunchApp::Explorer => "explorer.exe",
        LaunchApp::Notepad => "System32/notepad.exe",
        LaunchApp::VsCode => unreachable!(),
    };
    Ok(PathBuf::from(root).join(name))
}

fn serve_input(
    stream: &mut Stream,
    handle: u64,
    process_id: u32,
    focus: bool,
    resize: bool,
    stop: &AtomicBool,
) -> Result<()> {
    use protocol::InputEvent;
    if find_window(handle, process_id)?.is_none() {
        return protocol::send(stream, &Response::Error("Janela indisponível".into()));
    }
    let ready = if focus && resize {
        Response::InputReadyWithFocusAndResize
    } else if focus {
        Response::InputReadyWithFocus
    } else {
        Response::InputReady
    };
    protocol::send(stream, &ready)?;
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
            InputEvent::Focus => focus_window(window),
            InputEvent::Resize { width, height } if resize => resize_window(window, width, height),
            InputEvent::Resize { .. } => Err("Redimensionamento não negociado".into()),
            InputEvent::Drag { from: (x, y), to } => {
                use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
                pointer_window(
                    window,
                    x,
                    y,
                    &[(MOUSEEVENTF_LEFTDOWN, 0), (MOUSEEVENTF_LEFTUP, 0)],
                    true,
                    Some(to),
                )
            }
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
    H264(Vec<u8>, u32),
}

type Latest = Arc<CaptureQueue>;

const BACKGROUND_CAPTURE_INTERVAL_MS: u32 = 66;

struct Capture {
    latest: Latest,
    hwnd: usize,
    scratch: Vec<u8>,
    last: Instant,
    gpu_enabled: bool,
    gpu_encoder: Option<crate::nvenc::Encoder>,
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    device_context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = (Latest, bool, usize);
    type Error = protocol::Error;

    fn new(context: Context<Self::Flags>) -> Result<Self> {
        Ok(Self {
            latest: context.flags.0,
            gpu_enabled: context.flags.1,
            hwnd: context.flags.2,
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
        use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

        let foreground = unsafe { GetForegroundWindow() } as usize;
        let minimum_interval = if foreground == self.hwnd {
            protocol::MIN_CAPTURE_INTERVAL_MS
        } else {
            BACKGROUND_CAPTURE_INTERVAL_MS
        };
        let interval_ms = self
            .latest
            .interval_ms
            .load(Ordering::Relaxed)
            .max(minimum_interval);
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
                        .map_err(|_| "Fila de captura indisponível")? = Some(CapturedPacket::H264(
                        unit,
                        protocol::duration_micros(started.elapsed()),
                    ));
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

#[derive(Default)]
struct RtcDiagnostics {
    frames: u32,
    capture_micros: u64,
    encode_micros: u64,
    dropped: u32,
    encoder: Option<protocol::EncoderMode>,
}

impl RtcDiagnostics {
    fn record(&mut self, mode: protocol::EncoderMode, metrics: protocol::HostFrameMetrics) {
        self.frames += 1;
        self.capture_micros += u64::from(metrics.capture_micros);
        self.encode_micros += u64::from(metrics.encode_micros);
        self.dropped = self.dropped.saturating_add(metrics.dropped);
        self.encoder = Some(mode);
    }

    fn send(&mut self, stream: &mut Stream) -> Result<()> {
        let Some(encoder) = self.encoder else {
            return Ok(());
        };
        let sample = std::mem::take(self);
        let frames = u64::from(sample.frames);
        let metrics = protocol::HostFrameMetrics {
            capture_micros: (sample.capture_micros / frames) as u32,
            encode_micros: (sample.encode_micros / frames) as u32,
            dropped: sample.dropped,
        };
        protocol::send_h264_access_unit(stream, encoder.marker())?;
        protocol::send_h264_access_unit(stream, &metrics.h264_marker())
    }
}

fn capture(
    window: Window,
    stream: &mut Stream,
    stop: &AtomicBool,
    metrics: bool,
    h264: bool,
    h264_metrics: bool,
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
    let hwnd = window.as_raw_hwnd() as usize;
    let settings = Settings::new(
        window,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        (latest.clone(), h264, hwnd),
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
        let mut rtc_diagnostics = (h264_metrics && rtc.is_some()).then(RtcDiagnostics::default);
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
                    if let Some(diagnostics) = rtc_diagnostics.as_mut() {
                        diagnostics.send(stream)?;
                    }
                    protocol::heartbeat(stream)?;
                    last_heartbeat = Instant::now();
                }
            }
            let frame = latest
                .newest
                .lock()
                .map_err(|_| "Fila de captura indisponível")?
                .take();
            if let Some(CapturedPacket::H264(unit, encode_micros)) = frame {
                let dropped = latest.dropped.swap(0, Ordering::Relaxed);
                let sample = protocol::HostFrameMetrics {
                    capture_micros: 0,
                    encode_micros,
                    dropped,
                };
                if let Some(peer) = rtc.as_mut() {
                    peer.send(unit)?;
                    if let Some(diagnostics) = rtc_diagnostics.as_mut() {
                        diagnostics.record(protocol::EncoderMode::NvencGpuCopy, sample);
                    }
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
                    h264_metrics.then_some(sample),
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
                    let encode_start = Instant::now();
                    let encoded = encode_h264_frame(&mut encoder, &frame, bitrate_limit);
                    let encode_micros = protocol::duration_micros(encode_start.elapsed());
                    match encoded {
                        Ok((unit, mode)) => {
                            let sample = protocol::HostFrameMetrics {
                                capture_micros,
                                encode_micros,
                                dropped,
                            };
                            if let Some(peer) = rtc.as_mut() {
                                peer.send(unit)?;
                                if let Some(diagnostics) = rtc_diagnostics.as_mut() {
                                    diagnostics.record(mode, sample);
                                }
                                continue;
                            }
                            if !announced {
                                protocol::send(stream, &Response::StreamingH264)?;
                                announced = true;
                            }
                            send_h264(
                                stream,
                                &unit,
                                mode,
                                metrics,
                                h264_metrics.then_some(sample),
                                &mut last_encoder,
                            )?;
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
    frame_metrics: Option<protocol::HostFrameMetrics>,
    last: &mut Option<protocol::EncoderMode>,
) -> Result<()> {
    if metrics && *last != Some(mode) {
        protocol::send_h264_access_unit(stream, mode.marker())?;
        *last = Some(mode);
    }
    if let Some(metrics) = frame_metrics {
        protocol::send_h264_access_unit(stream, &metrics.h264_marker())?;
    }
    protocol::send_h264_access_unit(stream, unit)
}

fn resize_window(window: Window, width: u32, height: u32) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        UI::WindowsAndMessaging::{
            GetWindowRect, IsIconic, IsZoomed, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE,
            SWP_NOZORDER, SetWindowPos,
        },
    };
    if width < 64
        || height < 64
        || u64::from(width) * u64::from(height) > protocol::MAX_PIXELS as u64
    {
        return Err("Dimensões de janela fora do limite".into());
    }
    let _guard = INPUT_LOCK.lock().map_err(|_| "Entrada indisponível")?;
    let hwnd = window.as_raw_hwnd();
    let (mut outer, mut visible) = (RECT::default(), RECT::default());
    unsafe {
        if IsIconic(hwnd) != 0
            || IsZoomed(hwnd) != 0
            || GetWindowRect(hwnd, &mut outer) == 0
            || DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut visible as *mut RECT).cast(),
                size_of::<RECT>() as u32,
            ) < 0
        {
            return Err("Restaure a janela no Windows antes de redimensionar".into());
        }
        let extra_width = (outer.right - outer.left) - (visible.right - visible.left);
        let extra_height = (outer.bottom - outer.top) - (visible.bottom - visible.top);
        if SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            width as i32 + extra_width.max(0),
            height as i32 + extra_height.max(0),
            SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

fn focus_window(window: Window) -> Result<()> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IsIconic, SetForegroundWindow,
    };

    let _guard = INPUT_LOCK.lock().map_err(|_| "Entrada indisponível")?;
    let hwnd = window.as_raw_hwnd();
    unsafe {
        if IsIconic(hwnd) != 0 {
            return Err("Restaure a janela no Windows antes de focá-la".into());
        }
        if GetForegroundWindow() != hwnd {
            SetForegroundWindow(hwnd);
        }
        if GetForegroundWindow() != hwnd {
            return Err("O Windows não permitiu focar a janela".into());
        }
    }
    Ok(())
}

fn click_window(window: Window, x: u16, y: u16, button: protocol::PointerButton) -> Result<()> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let flags = match button {
        protocol::PointerButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        protocol::PointerButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        protocol::PointerButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };
    pointer_window(window, x, y, &[(flags.0, 0), (flags.1, 0)], true, None)
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
    pointer_window(window, x, y, &events, false, None)
}

fn pointer_window(
    window: Window,
    x: u16,
    y: u16,
    events: &[(u32, i32)],
    click: bool,
    destination: Option<(u16, u16)>,
) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{POINT, RECT},
        Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        UI::{
            Input::KeyboardAndMouse::*,
            WindowsAndMessaging::{
                GA_ROOT, GetAncestor, GetForegroundWindow, GetSystemMetrics, IsIconic,
                SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
                SetCursorPos, SetForegroundWindow, WindowFromPoint,
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
        let window_point = |x, y| POINT {
            x: rect.left + offset(x, rect.right - rect.left),
            y: rect.top + offset(y, rect.bottom - rect.top),
        };
        let point = window_point(x, y);
        if GetForegroundWindow() != hwnd || GetAncestor(WindowFromPoint(point), GA_ROOT) != hwnd {
            return Err("Traga a janela para frente no Windows".into());
        }
        if SetCursorPos(point.x, point.y) == 0
            || GetForegroundWindow() != hwnd
            || GetAncestor(WindowFromPoint(point), GA_ROOT) != hwnd
        {
            return Err("O destino do ponteiro mudou".into());
        }
        let mut inputs: Vec<_> = events
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
        if let Some((x, y)) = destination {
            let end = window_point(x, y);
            if GetAncestor(WindowFromPoint(end), GA_ROOT) != hwnd {
                return Err("O destino do arraste está fora da janela".into());
            }
            let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            if width < 2 || height < 2 {
                return Err("Área de trabalho virtual indisponível".into());
            }
            let absolute = |pixel: i32, origin: i32, length: i32| {
                // Aim for the center of the destination pixel in the normalized desktop.
                let offset = i64::from(pixel) - i64::from(origin);
                ((2 * offset + 1) * 32768 / i64::from(length)).clamp(0, 65535) as i32
            };
            inputs.insert(
                1,
                INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: absolute(end.x, GetSystemMetrics(SM_XVIRTUALSCREEN), width),
                            dy: absolute(end.y, GetSystemMetrics(SM_YVIRTUALSCREEN), height),
                            dwFlags: MOUSEEVENTF_MOVE
                                | MOUSEEVENTF_ABSOLUTE
                                | MOUSEEVENTF_VIRTUALDESK,
                            ..Default::default()
                        },
                    },
                },
            );
        }
        let sent = SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        );
        if sent != inputs.len() as u32 {
            if click && sent != 0 {
                SendInput(1, &inputs[inputs.len() - 1], size_of::<INPUT>() as i32);
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
