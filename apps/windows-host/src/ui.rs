use crate::host;
use gpui::{
    App, Application, Bounds, ClipboardItem, Context, Render, Task, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use protocol::{DEFAULT_PORT, PairingState};
use std::{
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
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
                size(px(680.), px(510.)),
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
    server: host::Server,
    address: SocketAddr,
    state: PairingState,
    _watch: Task<()>,
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
                self.status =
                    "Escolha a rede local ou use um túnel que aponta para este PC.".into();
            }
            Err(error) => self.status = format!("Falha ao listar interfaces · {error}"),
        }
    }

    fn start(&mut self, ip: Ipv4Addr, cx: &mut Context<Self>) {
        let address = SocketAddr::from((ip, DEFAULT_PORT));
        let result = (|| -> protocol::Result<_> {
            let directory = data_dir()?;
            fs::create_dir_all(&directory)?;
            host::start(address, &directory)
        })();
        match result {
            Ok(server) => {
                self.status = if ip.is_loopback() {
                    format!(
                        "Túnel local ativo em {address}. Use o endereço público do túnel no Mac."
                    )
                } else {
                    format!("Compartilhando em {address}")
                };
                self.sharing = Some(Sharing {
                    server,
                    address,
                    state: PairingState::Closed,
                    _watch: cx.spawn(async move |view, cx| {
                        loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(200))
                                .await;
                            let updated = view.update(cx, |this, cx| {
                                let Some(sharing) = &mut this.sharing else {
                                    return false;
                                };
                                let state = sharing.server.pairing_gate.state();
                                if sharing.state != state {
                                    sharing.state = state;
                                    cx.notify();
                                }
                                true
                            });
                            if !updated.unwrap_or(false) {
                                break;
                            }
                        }
                    }),
                });
            }
            Err(error) => self.status = format!("Falha ao iniciar · {error}"),
        }
        cx.notify();
    }

    fn pairing_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(sharing) = &self.sharing else {
            return div();
        };
        match &sharing.state {
            PairingState::Closed => div().child(button("pair", "Autorizar novo Mac").on_click(
                cx.listener(|this, _, _, cx| {
                    if let Some(sharing) = &mut this.sharing {
                        match sharing.server.pairing_gate.open() {
                            Ok(()) => sharing.state = sharing.server.pairing_gate.state(),
                            Err(error) => this.status = error.to_string(),
                        }
                        cx.notify();
                    }
                }),
            )),
            PairingState::Open => div()
                .text_sm()
                .child("Digite o endereço no Mac. O pareamento fica aberto por 90 segundos.")
                .child(
                    div()
                        .mt_2()
                        .child(
                            button("cancel", "Cancelar pareamento").on_click(cx.listener(
                                |this, _, _, cx| {
                                    if let Some(sharing) = &mut this.sharing {
                                        sharing.server.pairing_gate.close();
                                        sharing.state = PairingState::Closed;
                                        cx.notify();
                                    }
                                },
                            )),
                        ),
                ),
            PairingState::Pending(code) => {
                let accepted = code.clone();
                let rejected = code.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .child("Confira se este código aparece no Mac antes de aprovar."),
                    )
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(code.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(button("accept", "Aprovar Mac").on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.decide(&accepted, true, cx);
                                },
                            )))
                            .child(button("reject", "Recusar").on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.decide(&rejected, false, cx);
                                },
                            ))),
                    )
            }
        }
    }

    fn decide(&mut self, code: &str, accepted: bool, cx: &mut Context<Self>) {
        if let Some(sharing) = &mut self.sharing {
            sharing.server.pairing_gate.decide(code, accepted);
            sharing.state = sharing.server.pairing_gate.state();
            cx.notify();
        }
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
            .when_some(
                self.sharing.as_ref().map(|sharing| sharing.address),
                |panel, address| {
                    panel
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .when(!address.ip().is_loopback(), |bar| {
                                    bar.child(button("copy", "Copiar endereço").on_click(
                                        cx.listener(move |_, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                address.to_string(),
                                            ));
                                        }),
                                    ))
                                })
                                .child(button("stop", "Parar compartilhamento").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.sharing = None;
                                        this.refresh();
                                        cx.notify();
                                    }),
                                )),
                        )
                        .child(self.pairing_controls(cx))
                },
            )
            .when(self.sharing.is_none(), |panel| {
                panel
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(button("tunnel", "Usar túnel local").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.start(Ipv4Addr::LOCALHOST, cx);
                                },
                            )))
                            .child(button("refresh", "Atualizar rede").on_click(cx.listener(
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
                                        cx.listener(move |this, _, _, cx| this.start(ip, cx)),
                                    )
                                },
                            )),
                    )
            })
    }
}
