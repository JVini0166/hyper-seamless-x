use gpui::{
    App, Application, Bounds, Context, ObjectFit, PathPromptOptions, Render, RenderImage,
    SharedString, Task, TitlebarOptions, Window, WindowBounds, WindowOptions, div, img, prelude::*,
    px, rgb, size,
};
use protocol::{
    ConnectionProfile, Frame, Hello, LaunchApp, Pairing, Request, Response, Result, WindowAction,
    WindowInfo,
};
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use std::{
    fs,
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

#[derive(Clone)]
struct Host {
    addr: SocketAddr,
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

    fn request(
        &self,
        request: Request,
    ) -> Result<(Response, StreamOwned<ClientConnection, TcpStream>)> {
        let socket = TcpStream::connect_timeout(&self.addr, Duration::from_secs(4))?;
        socket.set_read_timeout(Some(Duration::from_secs(8)))?;
        socket.set_write_timeout(Some(Duration::from_secs(4)))?;
        socket.set_nodelay(true)?;
        let conn = ClientConnection::new(self.config.clone(), protocol::SERVER_NAME.try_into()?)?;
        let mut stream = StreamOwned::new(conn, socket);
        protocol::send(
            &mut stream,
            &Hello {
                version: protocol::VERSION,
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

fn profile_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").ok_or("Pasta pessoal indisponível")?)
            .join("Library/Application Support/HyperSeamlessX/pairing.json"),
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
        Ok(None) => (None, "Importe o pareamento exportado pelo Windows.".into()),
        Err(error) => (None, format!("Falha ao carregar o pareamento · {error}")),
    };
    Application::new().run(move |cx| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
        let options = window_options(cx, "Hyper Seamless X", 680., 540.);
        if let Err(error) = cx.open_window(options, |_, cx| {
            cx.new(|cx| {
                let mut launcher = Launcher {
                    host,
                    windows: Vec::new(),
                    status,
                    task: None,
                    importing: false,
                };
                launcher.refresh(None, cx);
                launcher
            })
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

struct Launcher {
    host: Option<Host>,
    windows: Vec<WindowInfo>,
    status: String,
    task: Option<Task<()>>,
    importing: bool,
}

impl Launcher {
    fn import(&mut self, cx: &mut Context<Self>) {
        if self.importing {
            return;
        }
        self.importing = true;
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Importar pareamento".into()),
        });
        cx.spawn(async move |view, cx| {
            let selected = picker.await;
            let result = (|| -> Result<_> {
                let Some(paths) = selected?? else {
                    return Ok(None);
                };
                let path = paths.first().ok_or("Nenhum arquivo selecionado")?;
                let profile = read_profile(path)?;
                let host = Host::from_profile(profile.clone())?;
                save_profile(&profile)?;
                Ok(Some(host))
            })();
            let _ = view.update(cx, |view, cx| {
                view.importing = false;
                match result {
                    Ok(Some(host)) => {
                        view.task = None;
                        view.windows.clear();
                        view.host = Some(host);
                        view.refresh(None, cx);
                    }
                    Ok(None) => {}
                    Err(error) => view.status = format!("Falha ao importar · {error}"),
                }
                cx.notify();
            });
        })
        .detach();
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
                        this.status = if windows.is_empty() {
                            "Nenhuma janela capturável encontrada".into()
                        } else {
                            format!("{} janelas disponíveis", windows.len())
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
                    .child(
                        button("import", "Importar pareamento")
                            .on_click(cx.listener(|this, _, _, cx| this.import(cx))),
                    )
                    .when(self.host.is_some(), |bar| {
                        bar.child(
                            button("refresh", "Atualizar")
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(None, cx))),
                        )
                        .child(button("explorer", "Abrir Explorador").on_click(cx.listener(
                            |this, _, _, cx| this.refresh(Some(LaunchApp::Explorer), cx),
                        )))
                        .child(
                            button("notepad", "Abrir Bloco de Notas").on_click(cx.listener(
                                |this, _, _, cx| this.refresh(Some(LaunchApp::Notepad), cx),
                            )),
                        )
                    }),
            )
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
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let options = window_options(cx, &info.title, 900., 600.);
                                let Some(host) = this.host.clone() else {
                                    return;
                                };
                                let info = info.clone();
                                if let Err(error) = cx.open_window(options, |window, cx| {
                                    cx.new(|cx| RemoteView::connect(host, info, window, cx))
                                }) {
                                    this.status = error.to_string();
                                    cx.notify();
                                }
                            }))
                    })),
            )
    }
}

struct RemoteView {
    frame: Option<Arc<RenderImage>>,
    status: SharedString,
    _task: Task<()>,
    host: Host,
    info: WindowInfo,
    action: Option<Task<()>>,
    action_error: Option<String>,
}

enum Update {
    Image(Arc<RenderImage>),
    Closed(String),
}

fn read_video(host: Host, info: WindowInfo, updates: &async_channel::Sender<Update>) -> Result<()> {
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
            if updates.force_send(Update::Image(Arc::new(data))).is_err() {
                break;
            }
        }
    }
    Ok(())
}

impl RemoteView {
    fn connect(host: Host, info: WindowInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (sender, receiver) = async_channel::bounded(1);
        let (video_host, video_info) = (host.clone(), info.clone());
        thread::spawn(move || {
            if let Err(error) = read_video(video_host, video_info, &sender) {
                let _ =
                    sender.force_send(Update::Closed(format!("Transmissão encerrada · {error}")));
            }
        });
        let task = cx.spawn_in(window, async move |view, cx| {
            while let Ok(update) = receiver.recv().await {
                if view
                    .update_in(cx, |this, window, cx| {
                        if let Some(previous) = this.frame.take() {
                            let _ = window.drop_image(previous);
                        }
                        match update {
                            Update::Image(frame) => this.frame = Some(frame),
                            Update::Closed(message) => this.status = message.into(),
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            frame: None,
            status: "Aguardando imagem do Windows…".into(),
            _task: task,
            host,
            info,
            action: None,
            action_error: None,
        }
    }
}

impl RemoteView {
    fn control(&mut self, action: WindowAction, cx: &mut Context<Self>) {
        if self.action.is_some() {
            return;
        }
        let host = self.host.clone();
        let request = Request::Control {
            handle: self.info.handle,
            process_id: self.info.process_id,
            action,
        };
        let operation = cx.background_executor().spawn(async move {
            match host.request(request)?.0 {
                Response::Controlled => Ok(()),
                _ => Err("Resposta de controle inesperada".into()),
            }
        });
        self.action = Some(cx.spawn(async move |view, cx| {
            let result: Result<()> = operation.await;
            let _ = view.update(cx, |view, cx| {
                view.action = None;
                view.action_error = result.err().map(|error| error.to_string());
                cx.notify();
            });
        }));
    }
}

impl Render for RemoteView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = [
            ("restore", "Restaurar", WindowAction::Restore),
            ("minimize", "Minimizar", WindowAction::Minimize),
            ("maximize", "Maximizar", WindowAction::Maximize),
            ("close", "Fechar no Windows", WindowAction::Close),
        ];
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x101722))
            .text_color(rgb(0xe8edf5))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .p_2()
                    .text_sm()
                    .children(actions.into_iter().map(|(id, label, action)| {
                        button(id, label)
                            .on_click(cx.listener(move |view, _, _, cx| view.control(action, cx)))
                    })),
            )
            .when_some(self.action_error.clone(), |view, error| {
                view.child(
                    div()
                        .px_3()
                        .text_sm()
                        .text_color(rgb(0xffae9c))
                        .child(error),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when_some(self.frame.clone(), |view, frame| {
                        view.child(img(frame).size_full().object_fit(ObjectFit::Contain))
                    })
                    .when(self.frame.is_none(), |view| view.child(self.status.clone())),
            )
    }
}
