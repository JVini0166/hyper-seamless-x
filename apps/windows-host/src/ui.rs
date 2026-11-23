use crate::host;
use gpui::{
    App, Application, Bounds, Context, Render, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};
use protocol::{ConnectionProfile, DEFAULT_PORT};
use std::{
    fs::{self, OpenOptions},
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
};

pub fn run() {
    Application::new().run(|cx| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(620.), px(475.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Hyper Seamless X · Host Windows".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let _ = cx.open_window(options, |_, cx| cx.new(|_| HostPanel::new()));
    });
}

struct Sharing {
    _server: host::Server,
    profile: ConnectionProfile,
}

struct HostPanel {
    addresses: Vec<(String, Ipv4Addr)>,
    sharing: Option<Sharing>,
    status: String,
}

impl HostPanel {
    fn new() -> Self {
        let mut panel = Self {
            addresses: Vec::new(),
            sharing: None,
            status: String::new(),
        };
        panel.refresh();
        panel
    }

    fn refresh(&mut self) {
        match if_addrs::get_if_addrs() {
            Ok(interfaces) => {
                self.addresses = interfaces
                    .into_iter()
                    .filter_map(|interface| match interface.ip() {
                        std::net::IpAddr::V4(ip)
                            if !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() =>
                        {
                            Some((interface.name, ip))
                        }
                        _ => None,
                    })
                    .collect();
                self.addresses.sort();
                self.addresses.dedup_by_key(|(_, ip)| *ip);
                self.status = if self.addresses.is_empty() {
                    "Nenhum endereço IPv4 disponível. Conecte o Windows à rede local.".into()
                } else {
                    "Selecione o endereço da rede que o Mac usa para iniciar o compartilhamento."
                        .into()
                };
            }
            Err(error) => self.status = format!("Falha ao listar interfaces · {error}"),
        }
    }

    fn start(&mut self, ip: Ipv4Addr) {
        let result = (|| -> protocol::Result<_> {
            let directory = data_dir()?;
            fs::create_dir_all(&directory)?;
            let address = SocketAddr::from((ip, DEFAULT_PORT));
            let (server, pairing) = host::start(address, &directory)?;
            Ok(Sharing {
                _server: server,
                profile: ConnectionProfile { address, pairing },
            })
        })();
        match result {
            Ok(sharing) => {
                self.status = format!("Compartilhando em {}", sharing.profile.address);
                self.sharing = Some(sharing);
            }
            Err(error) => self.status = format!("Falha ao iniciar · {error}"),
        }
    }

    fn export(&self, cx: &mut Context<Self>) {
        let Some(sharing) = &self.sharing else {
            return;
        };
        let profile = sharing.profile.clone();
        let directory = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_default();
        let picker = cx.prompt_for_new_path(&directory, Some("hyper-seamless-x-pairing.json"));
        cx.spawn(async move |view, cx| {
            let picker_result = picker.await;
            let result = (|| -> protocol::Result<_> {
                let Some(path) = picker_result?? else {
                    return Ok(None);
                };
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                if let Err(error) = serde_json::to_writer(&mut file, &profile) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error.into());
                }
                Ok(Some(path))
            })();
            let _ = view.update(cx, |view, cx| {
                match result {
                    Ok(Some(path)) => {
                        view.status = format!("Pareamento salvo em {}", path.display())
                    }
                    Ok(None) => {}
                    Err(error) => view.status = format!("Falha ao salvar · {error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn data_dir() -> protocol::Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA indisponível")?)
            .join("HyperSeamlessX"),
    )
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .rounded_md()
        .px_3()
        .py_2()
        .bg(rgb(0x233348))
        .hover(|style| style.bg(rgb(0x36516e)))
        .cursor_pointer()
        .child(label.into())
}

impl Render for HostPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .bg(rgb(0x101722))
            .text_color(rgb(0xe8edf5))
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Compartilhar janelas"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xa5b5c9))
                    .child(self.status.clone()),
            )
            .when(self.sharing.is_some(), |panel| {
                panel
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                button("export", "Exportar pareamento")
                                    .on_click(cx.listener(|this, _, _, cx| this.export(cx))),
                            )
                            .child(
                                button("stop", "Parar compartilhamento").on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.sharing = None;
                                        this.refresh();
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(
                        div().text_sm().text_color(rgb(0xa5b5c9)).child(
                            "Transfira o arquivo de pareamento ao Mac por um canal privado.",
                        ),
                    )
            })
            .when(self.sharing.is_none(), |panel| {
                panel
                    .child(
                        div().child(button("refresh", "Atualizar rede").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.refresh();
                                cx.notify();
                            },
                        ))),
                    )
                    .child(
                        div()
                            .id("addresses")
                            .flex_1()
                            .min_h_0()
                            .flex_col()
                            .flex()
                            .gap_2()
                            .overflow_y_scroll()
                            .children(self.addresses.iter().enumerate().map(
                                |(index, (name, ip))| {
                                    let ip = *ip;
                                    button(("listen", index), format!("{name} · {ip}")).on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.start(ip);
                                            cx.notify();
                                        }),
                                    )
                                },
                            )),
                    )
            })
    }
}
