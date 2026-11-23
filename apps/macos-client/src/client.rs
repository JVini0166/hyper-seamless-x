use gpui::{
    App, Application, Bounds, Context, ObjectFit, Render, RenderImage, SharedString, Task,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, img, prelude::*, px, rgb, size,
};
use protocol::{Frame, Hello, LaunchApp, Pairing, Request, Response, Result, WindowInfo};
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use std::{
    fs,
    net::{SocketAddr, TcpStream},
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

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [addr, pairing] = args.as_slice() else {
        return Err("Uso: macos-client <IP:porta> <pairing.json>".into());
    };
    let pairing: Pairing = serde_json::from_slice(&fs::read(pairing)?)?;
    let host = Host {
        addr: addr.parse()?,
        config: Arc::new(pairing.client_config()?),
        pairing,
    };
    Application::new().run(move |cx| {
        cx.activate(true);
        let options = window_options(cx, "Hyper Seamless X", 680., 540.);
        if let Err(error) = cx.open_window(options, |_, cx| {
            cx.new(|cx| {
                let mut launcher = Launcher {
                    host,
                    windows: Vec::new(),
                    status: String::new(),
                    task: None,
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
    host: Host,
    windows: Vec<WindowInfo>,
    status: String,
    task: Option<Task<()>>,
}

impl Launcher {
    fn refresh(&mut self, launch: Option<LaunchApp>, cx: &mut Context<Self>) {
        if self.task.is_some() {
            return;
        }
        self.status = "Conectando ao Windows…".into();
        let host = self.host.clone();
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
                        button("refresh", "Atualizar")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(None, cx))),
                    )
                    .child(button("explorer", "Abrir Explorador").on_click(
                        cx.listener(|this, _, _, cx| this.refresh(Some(LaunchApp::Explorer), cx)),
                    ))
                    .child(button("notepad", "Abrir Bloco de Notas").on_click(
                        cx.listener(|this, _, _, cx| this.refresh(Some(LaunchApp::Notepad), cx)),
                    )),
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
                                let host = this.host.clone();
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
        thread::spawn(move || {
            if let Err(error) = read_video(host, info, &sender) {
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
        }
    }
}

impl Render for RemoteView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x101722))
            .text_color(rgb(0xe8edf5))
            .when_some(self.frame.clone(), |view, frame| {
                view.child(img(frame).size_full().object_fit(ObjectFit::Contain))
            })
            .when(self.frame.is_none(), |view| view.child(self.status.clone()))
    }
}
