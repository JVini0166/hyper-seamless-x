use gpui::{
    App, Application, Bounds, Context, Entity, ObjectFit, Render, RenderImage, SharedString,
    Subscription, Task, TitlebarOptions, Window, WindowBounds, WindowOptions, div, img, point,
    prelude::*, px, rgb, size,
};
use gpui_component::{
    Root, Theme, ThemeMode,
    input::{Input, InputEvent, InputState},
};
use protocol::{
    ConnectionProfile, Endpoint, Frame, Hello, LaunchApp, Pairing, Request, Response, Result,
    VERSION, WindowInfo, Wire,
};
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Host {
    addr: Endpoint,
    pairing: Pairing,
    config: Arc<ClientConfig>,
}

impl Host {
    fn from_profile(profile: ConnectionProfile) -> Result<Self> {
        Ok(Self {
            addr: profile.address,
            config: Arc::new(profile.pairing.client_config()?),
            pairing: profile.pairing,
        })
    }

    fn request(&self, request: Request) -> Result<(Response, StreamOwned<ClientConnection, Wire>)> {
        let socket = self.addr.connect(Duration::from_secs(8))?;
        let conn = ClientConnection::new(self.config.clone(), protocol::SERVER_NAME.try_into()?)?;
        let mut stream = StreamOwned::new(conn, socket);
        protocol::send(
            &mut stream,
            &Hello {
                version: VERSION,
                token: self.pairing.token,
                request,
            },
        )?;
        match protocol::receive(&mut stream)? {
            Response::Error(message) => Err(message.into()),
            response => Ok((response, stream)),
        }
    }

    fn list(&self, launch: Option<LaunchApp>) -> Result<Vec<WindowInfo>> {
        if let Some(app) = launch {
            let (response, _) = self.request(Request::Launch(app))?;
            if !matches!(response, Response::Launched) {
                return Err("Resposta de abertura inesperada".into());
            }
            thread::sleep(Duration::from_millis(700));
        }
        match self.request(Request::List)?.0 {
            Response::Windows(windows) => Ok(windows),
            _ => Err("Resposta de listagem inesperada".into()),
        }
    }
}

enum EnrollmentUpdate {
    Code(String),
    Approved(ConnectionProfile),
    Failed(String),
}

fn enroll(address: Endpoint, updates: &async_channel::Sender<EnrollmentUpdate>) -> Result<()> {
    let profile = protocol::request_pairing(address, |code| {
        updates.send_blocking(EnrollmentUpdate::Code(code))?;
        Ok(())
    })?;
    updates.send_blocking(EnrollmentUpdate::Approved(profile))?;
    Ok(())
}

fn profile_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").ok_or("Pasta pessoal indisponível")?)
            .join("Library/Application Support/HyperSeamlessX/paired-host.json"),
    )
}

fn read_profile(path: &Path) -> Result<ConnectionProfile> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err("Arquivo de pareamento grande demais".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn save_profile(profile: &ConnectionProfile) -> Result<()> {
    let path = profile_path()?;
    let directory = path.parent().ok_or("Pasta de configuração indisponível")?;
    fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut file, profile)?;
    file.persist(path)?;
    Ok(())
}

pub fn run() -> Result<()> {
    let (host, status) = match profile_path().and_then(|path| {
        if !path.try_exists()? {
            return Ok(None);
        }
        Ok(Some(Host::from_profile(read_profile(&path)?)?))
    }) {
        Ok(Some(host)) => (Some(host), String::new()),
        Ok(None) => (None, "Digite o IP ou o link do host Windows.".into()),
        Err(error) => (None, format!("Falha ao carregar o pareamento · {error}")),
    };
    Application::new().run(move |cx| {
        gpui_component::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
        let options = window_options(cx, "Hyper Seamless X", 680., 540.);
        if let Err(error) = cx.open_window(options, |window, cx| {
            let existing = host.as_ref().map(|host| host.addr.to_string());
            let address = cx.new(|cx| {
                InputState::new(window, cx).placeholder("192.168.1.20 ou https://exemplo.ngrok.app")
            });
            if let Some(existing) = existing {
                address.update(cx, |field, cx| field.set_value(existing, window, cx));
            }
            let launcher = cx.new(|cx| {
                let enter = cx.subscribe(
                    &address,
                    |this: &mut Launcher, _, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            this.connect(cx);
                        }
                    },
                );
                let mut launcher = Launcher {
                    host,
                    address,
                    windows: Vec::new(),
                    opening: HashSet::new(),
                    status,
                    task: None,
                    enrollment: None,
                    _enter: enter,
                    discovered: BTreeMap::new(),
                    _discovery: watch_discovery(cx),
                };
                launcher.refresh(None, cx);
                launcher
            });
            cx.new(|cx| Root::new(launcher, window, cx))
        }) {
            eprintln!("Não foi possível abrir o launcher: {error}");
            cx.quit();
        }
    });
    Ok(())
}

fn window_options(cx: &App, title: &str, width: f32, height: f32) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(width), px(height)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(title.to_owned().into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

struct Enrollment {
    code: Option<String>,
    confirmed: bool,
    profile: Option<ConnectionProfile>,
    _task: Task<()>,
}

struct Launcher {
    host: Option<Host>,
    address: Entity<InputState>,
    windows: Vec<WindowInfo>,
    opening: HashSet<(u64, u32)>,
    status: String,
    task: Option<Task<()>>,
    enrollment: Option<Enrollment>,
    _enter: Subscription,
    discovered: BTreeMap<String, (String, SocketAddr)>,
    _discovery: Option<Task<()>>,
}

struct Discovery(mdns_sd::ServiceDaemon);

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

fn watch_discovery(cx: &Context<Launcher>) -> Option<Task<()>> {
    use mdns_sd::ServiceEvent;
    let daemon = Discovery(mdns_sd::ServiceDaemon::new().ok()?);
    let events = daemon.0.browse(protocol::DISCOVERY_SERVICE).ok()?;
    Some(cx.spawn(async move |view, cx| {
        let _daemon = daemon;
        let version = VERSION.to_string();
        while let Ok(event) = events.recv_async().await {
            let updated = view.update(cx, |view, cx| match event {
                ServiceEvent::ServiceResolved(service) => {
                    if service.get_property_val_str("version") != Some(version.as_str())
                        || service.get_port() == 0
                    {
                        return;
                    }
                    let Some(ip) = service
                        .get_addresses_v4()
                        .into_iter()
                        .filter(|ip| {
                            !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast()
                        })
                        .min()
                    else {
                        return;
                    };
                    let key = service.get_fullname();
                    if view.discovered.len() < 32 || view.discovered.contains_key(key) {
                        view.discovered.insert(
                            key.to_owned(),
                            (
                                mdns_sd::instance_name_unescaped(key),
                                SocketAddr::from((ip, service.get_port())),
                            ),
                        );
                        cx.notify();
                    }
                }
                ServiceEvent::ServiceRemoved(_, key) => {
                    if view.discovered.remove(&key).is_some() {
                        cx.notify();
                    }
                }
                _ => {}
            });
            if updated.is_err() {
                break;
            }
        }
    }))
}

impl Launcher {
    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.enrollment.is_some() {
            return;
        }
        let addr = match Endpoint::parse(self.address.read(cx).value().as_str()) {
            Ok(addr) => addr,
            Err(error) => {
                self.status = format!("Endereço inválido · {error}");
                cx.notify();
                return;
            }
        };
        if self.host.as_ref().is_some_and(|host| host.addr == addr) {
            self.refresh(None, cx);
            return;
        }
        let (sender, receiver) = async_channel::bounded(2);
        self.task = None;
        self.windows.clear();
        self.status = format!("Solicitando pareamento a {addr}…");
        thread::spawn(move || {
            if let Err(error) = enroll(addr, &sender) {
                let _ = sender.send_blocking(EnrollmentUpdate::Failed(error.to_string()));
            }
        });
        let task = cx.spawn(async move |view, cx| {
            while let Ok(update) = receiver.recv().await {
                let keep = view.update(cx, |view, cx| view.enrollment_update(update, cx));
                if !keep.unwrap_or(false) {
                    break;
                }
            }
        });
        self.enrollment = Some(Enrollment {
            code: None,
            confirmed: false,
            profile: None,
            _task: task,
        });
        cx.notify();
    }

    fn enrollment_update(&mut self, update: EnrollmentUpdate, cx: &mut Context<Self>) -> bool {
        let Some(enrollment) = &mut self.enrollment else {
            return false;
        };
        match update {
            EnrollmentUpdate::Code(code) => {
                enrollment.code = Some(code);
                self.status = "Compare o código com o exibido no Windows.".into();
            }
            EnrollmentUpdate::Approved(profile) => {
                enrollment.profile = Some(profile);
                if !enrollment.confirmed {
                    self.status =
                        "O Windows aprovou. Confirme que os dois códigos conferem.".into();
                }
                self.finish_enrollment(cx);
            }
            EnrollmentUpdate::Failed(error) => {
                self.status = format!("Falha no pareamento · {error}");
                self.enrollment = None;
            }
        }
        cx.notify();
        self.enrollment.is_some()
    }

    fn finish_enrollment(&mut self, cx: &mut Context<Self>) {
        let Some(enrollment) = &mut self.enrollment else {
            return;
        };
        if !enrollment.confirmed {
            return;
        }
        let Some(profile) = enrollment.profile.take() else {
            self.status = "Aguardando aprovação no Windows…".into();
            cx.notify();
            return;
        };
        self.enrollment = None;
        match Host::from_profile(profile.clone()).and_then(|host| {
            save_profile(&profile)?;
            Ok(host)
        }) {
            Ok(host) => {
                self.task = None;
                self.host = Some(host);
                self.refresh(None, cx);
            }
            Err(error) => self.status = format!("Falha ao salvar o pareamento · {error}"),
        }
        cx.notify();
    }

    fn forget_host(&mut self, cx: &mut Context<Self>) {
        let result = profile_path().and_then(|path| match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        });
        match result {
            Ok(()) => {
                self.task = None;
                self.host = None;
                self.windows.clear();
                self.status = "Autorize um novo pareamento no Windows e conecte novamente.".into();
            }
            Err(error) => self.status = format!("Falha ao esquecer endereço · {error}"),
        }
        cx.notify();
    }

    fn open_remote(&mut self, info: WindowInfo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.host.clone() else {
            return;
        };
        let key = (info.handle, info.process_id);
        if !self.opening.insert(key) {
            return;
        }
        let title = info.title.clone();
        let frames = start_video(host.clone(), info.clone());
        self.status = format!("Abrindo {title}…");
        cx.spawn_in(window, async move |view, cx| {
            let timeout = cx.background_executor().timer(Duration::from_secs(15));
            let first = futures_lite::future::race(async { frames.recv().await.ok() }, async {
                timeout.await;
                None
            })
            .await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.opening.remove(&key);
                match first {
                    Some(Update::Image(frame)) => {
                        let options = remote_options(window, &title, &frame, cx);
                        match cx.open_window(options, |window, cx| {
                            cx.new(|cx| RemoteView::new(frame, frames, host, info, window, cx))
                        }) {
                            Ok(_) => view.status = format!("{title} aberto"),
                            Err(error) => view.status = error.to_string(),
                        }
                    }
                    Some(Update::Closed(error) | Update::Reconnecting(error)) => {
                        view.status = error
                    }
                    None => {
                        view.status = format!(
                            "Sem imagem de {title}. Confira se a janela está aberta no Windows."
                        )
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn refresh(&mut self, launch: Option<LaunchApp>, cx: &mut Context<Self>) {
        if self.task.is_some() {
            return;
        }
        let Some(host) = self.host.clone() else {
            return;
        };
        self.status = format!("Conectando a {}…", host.addr);
        let request = cx
            .background_executor()
            .spawn(async move { host.list(launch) });
        self.task = Some(cx.spawn(async move |view, cx| {
            let result = request.await;
            let _ = view.update(cx, |this, cx| {
                this.task = None;
                match result {
                    Ok(windows) => {
                        this.status = match windows.len() {
                            0 => "Nenhuma janela capturável encontrada".into(),
                            1 => "1 janela disponível".into(),
                            count => format!("{count} janelas disponíveis"),
                        };
                        this.windows = windows;
                    }
                    Err(error) => {
                        this.status = format!("Falha na conexão · {error}");
                        this.windows.clear();
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

fn button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_2()
        .rounded_md()
        .cursor_pointer()
        .bg(rgb(0x212c3c))
        .hover(|style| style.bg(rgb(0x30435a)))
        .child(label)
}

impl Render for Launcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x101722))
            .text_color(rgb(0xe8edf5))
            .flex_col()
            .flex()
            .gap_4()
            .p_5()
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Janelas do Windows"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xa5b5c9))
                    .child(self.status.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(Input::new(&self.address)))
                    .child(
                        button("connect", "Conectar")
                            .on_click(cx.listener(|view, _, _, cx| view.connect(cx))),
                    ),
            )
            .when(
                self.host.is_none() && self.enrollment.is_none() && !self.discovered.is_empty(),
                |view| {
                    view.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0xa5b5c9))
                                    .child("Hosts na rede local"),
                            )
                            .child(
                                div()
                                    .id("lan-hosts")
                                    .max_h(px(190.))
                                    .overflow_y_scroll()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .children(self.discovered.iter().map(
                                        |(key, (name, address))| {
                                            let address = *address;
                                            div()
                                                .id(gpui::SharedString::from(format!("lan-{key}")))
                                                .rounded_md()
                                                .px_3()
                                                .py_2()
                                                .cursor_pointer()
                                                .bg(rgb(0x1a2432))
                                                .hover(|style| style.bg(rgb(0x26374b)))
                                                .child(format!("{name} · {address}"))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.address.update(cx, |field, cx| {
                                                            field.set_value(
                                                                address.to_string(),
                                                                window,
                                                                cx,
                                                            )
                                                        });
                                                        this.connect(cx);
                                                    },
                                                ))
                                        },
                                    )),
                            ),
                    )
                },
            )
            .when_some(self.enrollment.as_ref(), |view, enrollment| {
                view.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(0x1a2432))
                        .when_some(enrollment.code.clone(), |panel, code| {
                            panel.child(
                                div()
                                    .text_2xl()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(code),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .when(enrollment.code.is_some() && !enrollment.confirmed, |bar| {
                                    bar.child(button("confirm", "Os códigos conferem").on_click(
                                        cx.listener(|view, _, _, cx| {
                                            if let Some(enrollment) = &mut view.enrollment {
                                                enrollment.confirmed = true;
                                                view.finish_enrollment(cx);
                                            }
                                        }),
                                    ))
                                })
                                .child(button("cancel", "Cancelar").on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.enrollment = None;
                                        view.status = "Pareamento cancelado".into();
                                        cx.notify();
                                    },
                                ))),
                        ),
                )
            })
            .when(self.host.is_some() && self.enrollment.is_none(), |view| {
                view.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            button("refresh", "Atualizar")
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(None, cx))),
                        )
                        .child(button("explorer", "Abrir Explorador").on_click(cx.listener(
                            |this, _, _, cx| this.refresh(Some(LaunchApp::Explorer), cx),
                        )))
                        .child(button("notepad", "Abrir Bloco de Notas").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.refresh(Some(LaunchApp::Notepad), cx)
                            }),
                        ))
                        .child(
                            button("forget", "Esquecer host")
                                .on_click(cx.listener(|this, _, _, cx| this.forget_host(cx))),
                        ),
                )
            })
            .child(
                div()
                    .id("windows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(self.windows.iter().enumerate().map(|(index, info)| {
                        let info = info.clone();
                        div()
                            .id(("remote", index))
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(0x1a2432))
                            .hover(|style| style.bg(rgb(0x26374b)))
                            .cursor_pointer()
                            .child(div().child(info.title.clone()))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0xa5b5c9))
                                    .child(info.process.clone()),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_remote(info.clone(), window, cx);
                            }))
                    })),
            )
    }
}

struct InputSession {
    host: Host,
    target: (u64, u32),
    stream: Option<(Instant, StreamOwned<ClientConnection, Wire>)>,
}

impl InputSession {
    fn send(&mut self, queued: Instant, event: protocol::InputEvent) -> Result<()> {
        // The host expires idle readers after five seconds. Reconnect before that
        // threshold, and never replay an event after a write may have succeeded.
        let mut stream = match self.stream.take() {
            Some((last, stream)) if last.elapsed() < Duration::from_secs(3) => stream,
            _ => {
                let (handle, process_id) = self.target;
                let (response, stream) =
                    self.host.request(Request::Input { handle, process_id })?;
                if !matches!(response, Response::InputReady) {
                    return Err("Resposta de canal de entrada inesperada".into());
                }
                stream
            }
        };
        let sent_at = Instant::now();
        if sent_at.duration_since(queued) >= Duration::from_secs(1) {
            self.stream = Some((sent_at, stream));
            return Err("Entrada descartada após atraso na conexão".into());
        }
        protocol::send(&mut stream, &event)?;
        let response = protocol::receive(&mut stream)?;
        self.stream = Some((sent_at, stream));
        match response {
            Response::Controlled => Ok(()),
            Response::Error(message) => Err(message.into()),
            _ => Err("Resposta de entrada inesperada".into()),
        }
    }
}

struct RemoteView {
    frame: Option<VideoFrame>,
    status: SharedString,
    reconnecting: Option<String>,
    _task: Task<()>,
    _chrome_task: Task<()>,
    _input_task: Task<()>,
    input: async_channel::Sender<(Instant, protocol::InputEvent)>,
    focus: gpui::FocusHandle,
    scroll_remainder: (f64, f64),
    input_error: Option<String>,
}

#[derive(Clone)]
struct VideoFrame {
    image: Arc<RenderImage>,
    width: u32,
    height: u32,
}

enum Update {
    Image(VideoFrame),
    Reconnecting(String),
    Closed(String),
}

fn read_video(
    host: &Host,
    info: &WindowInfo,
    updates: &async_channel::Sender<Update>,
    delivered: &mut bool,
) -> Result<()> {
    let (response, mut stream) = host.request(Request::View {
        handle: info.handle,
        process_id: info.process_id,
    })?;
    if !matches!(response, Response::Streaming) {
        return Err("Resposta de captura inesperada".into());
    }
    while !updates.is_closed() {
        if let Some(frame) = Frame::receive(&mut stream)? {
            let mut pixels = frame.bgra;
            for pixel in pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            let buffer = image::RgbaImage::from_raw(frame.width, frame.height, pixels)
                .ok_or("Frame inválido")?;
            let data = RenderImage::new([image::Frame::new(buffer)]);
            if updates
                .force_send(Update::Image(VideoFrame {
                    image: Arc::new(data),
                    width: frame.width,
                    height: frame.height,
                }))
                .is_err()
            {
                break;
            }
            *delivered = true;
        }
    }
    Ok(())
}

fn start_video(host: Host, info: WindowInfo) -> async_channel::Receiver<Update> {
    let (sender, receiver) = async_channel::bounded(1);
    thread::spawn(move || {
        let mut has_opened = false;
        let mut retries = 0;
        loop {
            let mut delivered = false;
            let outcome = read_video(&host, &info, &sender, &mut delivered);
            let Err(error) = outcome else {
                break;
            };
            if sender.is_closed() {
                break;
            }
            if delivered {
                has_opened = true;
                retries = 0;
            }
            if !has_opened || retries == 5 || !transient_video_error(&error) {
                let _ = sender
                    .send_blocking(Update::Closed(format!("Transmissão encerrada · {error}")));
                break;
            }
            retries += 1;
            let wait = Duration::from_millis(250 << retries);
            if sender
                .send_blocking(Update::Reconnecting(format!(
                    "Reconectando à janela do Windows · {retries}/5"
                )))
                .is_err()
            {
                break;
            }
            let deadline = Instant::now() + wait;
            while !sender.is_closed() && Instant::now() < deadline {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(50)),
                );
            }
            if sender.is_closed() {
                break;
            }
        }
    });
    receiver
}

fn transient_video_error(error: &protocol::Error) -> bool {
    use std::io::ErrorKind::*;
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            UnexpectedEof
                | BrokenPipe
                | TimedOut
                | WouldBlock
                | ConnectionReset
                | ConnectionAborted
                | ConnectionRefused
                | NotConnected
                | NetworkUnreachable
                | HostUnreachable
        )
    })
}

impl RemoteView {
    fn new(
        frame: VideoFrame,
        receiver: async_channel::Receiver<Update>,
        host: Host,
        info: WindowInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        set_native_aspect(window, frame.width, frame.height);
        let target = (info.handle, info.process_id);
        let focus = cx.focus_handle();
        focus.focus(window);
        let (input, requests) = async_channel::bounded::<(Instant, protocol::InputEvent)>(16);
        let (results, feedback) = async_channel::bounded(1);
        thread::spawn(move || {
            let mut session = InputSession {
                host,
                target,
                stream: None,
            };
            while let Ok((queued, event)) = requests.recv_blocking() {
                if queued.elapsed() >= Duration::from_secs(1) {
                    continue;
                }
                if results
                    .force_send(
                        session
                            .send(queued, event)
                            .err()
                            .map(|error| error.to_string()),
                    )
                    .is_err()
                {
                    break;
                }
            }
        });
        let input_task = cx.spawn(async move |view, cx| {
            while let Ok(error) = feedback.recv().await {
                if view
                    .update(cx, |view, cx| {
                        view.input_error = error;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let task = cx.spawn_in(window, async move |view, cx| {
            while let Ok(update) = receiver.recv().await {
                if view
                    .update_in(cx, |this, window, cx| {
                        match update {
                            Update::Image(frame) => {
                                if let Some(previous) = this.frame.replace(frame) {
                                    let _ = window.drop_image(previous.image);
                                }
                                this.reconnecting = None;
                            }
                            Update::Reconnecting(message) => {
                                this.reconnecting = Some(message);
                                this.input_error = None;
                            }
                            Update::Closed(message) => {
                                if let Some(previous) = this.frame.take() {
                                    let _ = window.drop_image(previous.image);
                                }
                                this.reconnecting = None;
                                this.status = message.into();
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        set_native_buttons(window, false);
        let chrome_task = cx.spawn_in(window, async move |view, cx| {
            let mut visible = false;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
                if view
                    .update_in(cx, |_, window, _| {
                        let cursor = window.mouse_position();
                        let within_top = cursor.x >= px(0.)
                            && cursor.x < window.viewport_size().width
                            && cursor.y >= px(0.)
                            && cursor.y < px(if visible { 36. } else { 9. });
                        if within_top != visible {
                            set_native_buttons(window, within_top);
                            visible = within_top;
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            frame: Some(frame),
            status: "Aguardando imagem do Windows…".into(),
            reconnecting: None,
            _task: task,
            _chrome_task: chrome_task,
            _input_task: input_task,
            input,
            focus,
            scroll_remainder: (0., 0.),
            input_error: None,
        }
    }
}

fn remote_options(launcher: &Window, title: &str, frame: &VideoFrame, cx: &App) -> WindowOptions {
    let display = launcher.display(cx).or_else(|| cx.primary_display());
    let bounds = display.as_ref().map_or_else(
        || Bounds::new(point(px(0.), px(0.)), launcher.viewport_size()),
        |display| display.bounds(),
    );
    let work_area = display_work_area(launcher, bounds);
    let scale = launcher.scale_factor().max(1.);
    let width = frame.width as f32 / scale;
    let height = frame.height as f32 / scale;
    let fit = 1.0_f32
        .min((work_area.size.width.to_f64() as f32 - 24.).max(1.) / width)
        .min((work_area.size.height.to_f64() as f32 - 24.).max(1.) / height);
    let content = size(px(width * fit), px(height * fit));
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered_at(
            work_area.center(),
            content,
        ))),
        display_id: display.map(|display| display.id()),
        titlebar: Some(TitlebarOptions {
            title: Some(title.to_owned().into()),
            appears_transparent: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn appkit_window(window: &Window) -> Option<std::ptr::NonNull<objc2::runtime::AnyObject>> {
    use objc2::{msg_send, runtime::AnyObject};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // GPUI owns the AppKit view and calls this on the UI thread.
    let native: *mut AnyObject =
        unsafe { msg_send![handle.ns_view.as_ptr().cast::<AnyObject>(), window] };
    std::ptr::NonNull::new(native)
}

fn display_work_area(window: &Window, fallback: Bounds<gpui::Pixels>) -> Bounds<gpui::Pixels> {
    use objc2::{msg_send, runtime::AnyObject};
    use objc2_foundation::NSRect;
    let Some(native) = appkit_window(window) else {
        return fallback;
    };
    let screen: *mut AnyObject = unsafe { msg_send![native.as_ptr(), screen] };
    if screen.is_null() {
        return fallback;
    }
    let (outer, visible): (NSRect, NSRect) =
        unsafe { (msg_send![screen, frame], msg_send![screen, visibleFrame]) };
    let dx = visible.origin.x - outer.origin.x;
    let dy = outer.origin.y + outer.size.height - visible.origin.y - visible.size.height;
    Bounds::new(
        point(
            fallback.origin.x + px(dx as f32),
            fallback.origin.y + px(dy as f32),
        ),
        size(
            px(visible.size.width as f32),
            px(visible.size.height as f32),
        ),
    )
}

fn set_native_aspect(window: &Window, width: u32, height: u32) {
    use objc2::msg_send;
    use objc2_foundation::NSSize;
    if let Some(native) = appkit_window(window) {
        unsafe {
            let _: () = msg_send![native.as_ptr(), setContentAspectRatio: NSSize::new(f64::from(width), f64::from(height))];
        }
    }
}

fn set_native_buttons(window: &Window, visible: bool) {
    use objc2::{
        msg_send,
        runtime::{AnyObject, Bool},
    };
    let Some(native) = appkit_window(window) else {
        return;
    };
    unsafe {
        // NSWindowButton values for close, miniaturize, and zoom.
        for kind in [0usize, 1, 2] {
            let button: *mut AnyObject = msg_send![native.as_ptr(), standardWindowButton: kind];
            if !button.is_null() {
                let _: () = msg_send![button, setHidden: Bool::new(!visible)];
            }
        }
    }
}

impl RemoteView {
    fn click(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        button: protocol::PointerButton,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((x, y)) = self.pointer_position(position, window) {
            self.queue(protocol::InputEvent::Click { x, y, button }, cx);
        }
    }

    fn pointer_position(
        &self,
        position: gpui::Point<gpui::Pixels>,
        window: &Window,
    ) -> Option<(u16, u16)> {
        let frame = self.frame.as_ref()?;
        let viewport = window.viewport_size();
        let (width, height) = (f64::from(frame.width), f64::from(frame.height));
        let scale = (viewport.width.to_f64() / width).min(viewport.height.to_f64() / height);
        if scale <= 0. {
            return None;
        }
        let normalized = |position, viewport: gpui::Pixels, image: f64| {
            let rendered = image * scale;
            (position - (viewport.to_f64() - rendered) / 2.) / rendered
        };
        let x = normalized(position.x.to_f64(), viewport.width, width);
        let y = normalized(position.y.to_f64(), viewport.height, height);
        ((0. ..1.).contains(&x) && (0. ..1.).contains(&y))
            .then(|| ((x * 65535.).round() as u16, (y * 65535.).round() as u16))
    }

    fn scroll(&mut self, event: &gpui::ScrollWheelEvent, window: &Window, cx: &mut Context<Self>) {
        let Some((x, y)) = self.pointer_position(event.position, window) else {
            self.scroll_remainder = (0., 0.);
            return;
        };
        if matches!(event.touch_phase, gpui::TouchPhase::Started) {
            self.scroll_remainder = (0., 0.);
        }
        let delta = event.delta.pixel_delta(px(40.));
        let wheel_steps = |carry: &mut f64, delta: f64| {
            *carry += delta / 40.;
            let steps = carry.trunc();
            *carry -= steps;
            steps.clamp(-8., 8.) as i16
        };
        let horizontal = wheel_steps(&mut self.scroll_remainder.0, -delta.x.to_f64());
        let vertical = wheel_steps(&mut self.scroll_remainder.1, delta.y.to_f64());
        if horizontal != 0 || vertical != 0 {
            self.queue(
                protocol::InputEvent::Scroll {
                    x,
                    y,
                    horizontal,
                    vertical,
                },
                cx,
            );
        }
    }

    fn queue(&mut self, request: protocol::InputEvent, cx: &mut Context<Self>) {
        if self.reconnecting.is_some() {
            return;
        }
        if self.input.try_send((Instant::now(), request)).is_err() {
            self.input_error = Some("A fila de entrada está ocupada".into());
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        use protocol::RemoteKey::*;
        let stroke = &event.keystroke;
        if self.frame.is_none()
            || stroke.modifiers.platform
            || stroke.modifiers.control
            || stroke.modifiers.function
        {
            return;
        }
        let key = match stroke.key.as_str() {
            "backspace" => Backspace,
            "delete" => Delete,
            "enter" => Enter,
            "tab" => Tab,
            "escape" => Escape,
            "left" => Left,
            "right" => Right,
            "up" => Up,
            "down" => Down,
            "home" => Home,
            "end" => End,
            "pageup" => PageUp,
            "pagedown" => PageDown,
            _ => match stroke.key_char.as_deref() {
                Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                    Text(text.to_owned())
                }
                _ => return,
            },
        };
        self.queue(protocol::InputEvent::Key(key), cx);
    }
}

impl Render for RemoteView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, _, cx| this.key_down(event, cx)))
            .on_scroll_wheel(cx.listener(|view, event, window, cx| view.scroll(event, window, cx)))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, window, cx| {
                    this.focus.focus(window);
                    this.click(event.position, protocol::PointerButton::Left, window, cx);
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseUpEvent, window, cx| {
                    this.click(event.position, protocol::PointerButton::Right, window, cx);
                }),
            )
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x101722))
            .text_color(rgb(0xe8edf5))
            .when_some(self.frame.clone(), |view, frame| {
                view.child(img(frame.image).size_full().object_fit(ObjectFit::Contain))
            })
            .when(self.frame.is_none(), |view| view.child(self.status.clone()))
            .when_some(
                self.reconnecting
                    .clone()
                    .or_else(|| self.input_error.clone()),
                |view, error| {
                    view.child(
                        div()
                            .absolute()
                            .bottom_2()
                            .left_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(0x291719))
                            .text_sm()
                            .child(error),
                    )
                },
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn open_video_resumes_after_tls_transport_disconnects() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = Endpoint::parse(&listener.local_addr()?.to_string())?;
        let (pairing, key) = protocol::new_identity()?;
        let config = Arc::new(pairing.server_config(key)?);
        let host = Host::from_profile(ConnectionProfile {
            address,
            pairing: pairing.clone(),
        })?;
        let server = thread::spawn(move || -> Result<()> {
            for width in [2, 3] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => return Err(error.into()),
                    }
                };
                let mut stream = StreamOwned::new(
                    rustls::ServerConnection::new(config.clone())?,
                    protocol::use_incoming(socket)?,
                );
                let hello: Hello = protocol::receive(&mut stream)?;
                hello.verify(&pairing)?;
                assert!(matches!(
                    hello.request,
                    Request::View {
                        handle: 42,
                        process_id: 7
                    }
                ));
                protocol::send(&mut stream, &Response::Streaming)?;
                Frame {
                    width,
                    height: 1,
                    bgra: vec![0; width as usize * 4],
                }
                .send(&mut stream)?;
                // Dropping the socket without close_notify simulates a cut transport.
            }
            Ok(())
        });

        let updates = start_video(
            host,
            WindowInfo {
                handle: 42,
                process_id: 7,
                title: "Janela de teste".into(),
                process: "test.exe".into(),
            },
        );
        let (observed, events) = mpsc::sync_channel(4);
        let relay = thread::spawn(move || {
            while let Ok(update) = updates.recv_blocking() {
                let done = matches!(&update, Update::Image(frame) if frame.width == 3);
                if observed.send(update).is_err() || done {
                    break;
                }
            }
        });
        let next = || events.recv_timeout(Duration::from_secs(5));
        assert!(matches!(next()?, Update::Image(frame) if frame.width == 2));
        assert!(matches!(next()?, Update::Reconnecting(_)));
        assert!(matches!(next()?, Update::Image(frame) if frame.width == 3));
        relay.join().expect("video update relay panicked");
        server.join().expect("test host panicked")?;
        Ok(())
    }
}
