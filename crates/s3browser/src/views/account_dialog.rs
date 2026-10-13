//! 添加或编辑账户。自己画一个浮层，够用就行。

use gpui_kit::component::{button::*, input::*, *};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::views::main_window::MainWindow;
use s3browser::config::{Addressing, BucketView, Profile, new_profile_id};

pub struct AccountDialog {
    main: WeakEntity<MainWindow>,
    /// 编辑已有账户时是它的 id，新增时为空。
    editing: Option<String>,
    /// 编辑已有账户时原样带回去的东西：手填的桶名单、本地的顺序和隐藏。
    /// 对话框里没有这些的编辑界面，丢了就等于用户手改的配置被界面吃掉了。
    kept: (Vec<String>, BucketView),
    name: Entity<InputState>,
    endpoint: Entity<InputState>,
    region: Entity<InputState>,
    access_key: Entity<InputState>,
    secret_key: Entity<InputState>,
    addressing: Addressing,
    insecure_tls: bool,
    /// "测试连接"的结果，None 表示还没测过。
    probe: Option<Result<String, String>>,
    probing: bool,
}

impl AccountDialog {
    pub fn new(
        existing: Option<Profile>,
        main: WeakEntity<MainWindow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let s3 = existing.clone();
        let mut field = |value: &str, placeholder: &'static str, cx: &mut Context<Self>| {
            let value = value.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };

        let name = field(
            existing.as_ref().map(|p| p.name.as_str()).unwrap_or(""),
            "例如 本地 MinIO",
            cx,
        );
        let endpoint = field(
            s3.as_ref().map(|s| s.endpoint.as_str()).unwrap_or(""),
            "https://s3.example.com",
            cx,
        );
        let region = field(
            s3.as_ref().map(|s| s.region.as_str()).unwrap_or(""),
            "us-east-1",
            cx,
        );
        let access_key = field(
            s3.as_ref().map(|s| s.access_key.as_str()).unwrap_or(""),
            "Access Key",
            cx,
        );
        let secret_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Secret Key")
                .default_value(
                    s3.as_ref()
                        .map(|s| s.secret_key.clone())
                        .unwrap_or_default(),
                )
                .masked(true)
        });

        AccountDialog {
            main,
            editing: existing.as_ref().map(|p| p.id.clone()),
            kept: existing
                .as_ref()
                .map(|p| (p.buckets.clone(), p.view.clone()))
                .unwrap_or_default(),
            name,
            region,
            endpoint,
            access_key,
            secret_key,
            addressing: s3.as_ref().map(|s| s.addressing).unwrap_or_default(),
            insecure_tls: s3.as_ref().map(|s| s.insecure_tls).unwrap_or(false),
            probe: None,
            probing: false,
        }
    }

    fn to_profile(&self, cx: &App) -> Profile {
        let name = self.name.read(cx).value().to_string();
        Profile {
            id: self.editing.clone().unwrap_or_else(new_profile_id),
            name: if name.trim().is_empty() {
                "未命名账户".to_owned()
            } else {
                name
            },
            endpoint: self.endpoint.read(cx).value().trim().to_string(),
            region: self.region.read(cx).value().trim().to_string(),
            access_key: self.access_key.read(cx).value().trim().to_string(),
            secret_key: self.secret_key.read(cx).value().to_string(),
            addressing: self.addressing,
            insecure_tls: self.insecure_tls,
            payload_signing: Default::default(),
            buckets: self.kept.0.clone(),
            view: self.kept.1.clone(),
        }
    }

    /// 探一下通不通：调一次 ListBuckets。
    fn test_connection(&mut self, cx: &mut Context<Self>) {
        let profile = self.to_profile(cx);
        let Some(main) = self.main.upgrade() else {
            return;
        };
        let Some(client) = main.read(cx).app.read(cx).probe_client(&profile) else {
            self.probe = Some(Err("endpoint 填得不对".to_owned()));
            cx.notify();
            return;
        };

        self.probing = true;
        self.probe = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = client.list_buckets().await;
            this.update(cx, |this, cx| {
                this.probing = false;
                this.probe = Some(match result {
                    Ok(buckets) => Ok(format!("连接成功，{} 个桶", buckets.len())),
                    Err(e) => Err(e.to_string()),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let profile = self.to_profile(cx);
        let id = profile.id.clone();
        let Some(main) = self.main.upgrade() else {
            return;
        };
        main.update(cx, |main, cx| {
            main.app.update(cx, |app, cx| {
                app.config.upsert(profile);
                app.save_config();
                app.invalidate(&id);
                cx.notify();
            });
            main.dialog = None;
            cx.notify();
        });
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(main) = self.main.upgrade() {
            main.update(cx, |main, cx| {
                main.dialog = None;
                cx.notify();
            });
        }
    }

    fn labeled(
        &self,
        label: &'static str,
        input: &Entity<InputState>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(Input::new(input))
    }
}

impl Render for AccountDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.editing.is_some() {
            "编辑账户"
        } else {
            "添加账户"
        };

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.35))
            .child(
                v_flex()
                    .w(px(440.))
                    .p_4()
                    .gap_2()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(div().font_bold().child(title))
                    .child(self.labeled("名称", &self.name, cx))
                    .child(self.labeled("Endpoint", &self.endpoint, cx))
                    .child(
                        h_flex()
                            .gap_3()
                            .child(div().flex_1().child(self.labeled("区域", &self.region, cx)))
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("寻址方式"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(addressing_button(
                                                "addr-auto",
                                                "自动",
                                                Addressing::Auto,
                                                self.addressing,
                                                cx,
                                            ))
                                            .child(addressing_button(
                                                "addr-vhost",
                                                "子域名",
                                                Addressing::VirtualHost,
                                                self.addressing,
                                                cx,
                                            ))
                                            .child(addressing_button(
                                                "addr-path",
                                                "路径",
                                                Addressing::Path,
                                                self.addressing,
                                                cx,
                                            )),
                                    ),
                            ),
                    )
                    .child(self.labeled("Access Key", &self.access_key, cx))
                    .child(self.labeled("Secret Key", &self.secret_key, cx))
                    .child(
                        Button::new("insecure-tls")
                            .ghost()
                            .small()
                            .selected(self.insecure_tls)
                            .label(if self.insecure_tls {
                                "☑ 跳过证书校验"
                            } else {
                                "☐ 跳过证书校验"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.insecure_tls = !this.insecure_tls;
                                cx.notify();
                            })),
                    )
                    .when_some(self.probe.clone(), |this, probe| {
                        let (text, color) = match &probe {
                            Ok(message) => (message.clone(), cx.theme().green),
                            Err(message) => (message.clone(), cx.theme().red),
                        };
                        this.child(div().text_xs().text_color(color).child(text))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("test")
                                    .label(if self.probing {
                                        "测试中…"
                                    } else {
                                        "测试连接"
                                    })
                                    .ghost()
                                    .disabled(self.probing)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.test_connection(cx)
                                    })),
                            )
                            .child(
                                Button::new("cancel")
                                    .label("取消")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                            )
                            .child(
                                Button::new("save")
                                    .label("保存")
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            ),
                    ),
            )
    }
}

fn addressing_button(
    id: &'static str,
    label: &'static str,
    value: Addressing,
    current: Addressing,
    cx: &mut Context<AccountDialog>,
) -> impl IntoElement {
    Button::new(id)
        .label(label)
        .small()
        .ghost()
        .selected(current == value)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.addressing = value;
            cx.notify();
        }))
}
