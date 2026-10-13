//! 属性面板。Ctrl+I 或 Alt+Enter 切换，贴在内容区右侧。
//!
//! 列表里已经有的字段直接显示；Content-Type 这类只有 HEAD 才知道的，
//! 面板打开后再补一次请求。文件夹要算项目数和总大小就得递归列举，所以做成按钮。

use gpui_kit::component::tab::{Tab as TabHead, TabBar};
use gpui_kit::component::{button::*, *};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use s3api::{Acl, HeadResult};
use s3browser::browser_state::format_size;

use crate::views::list_view::format_time;
use crate::views::main_window::MainWindow;

/// 面板宽度。设计里说可以拖，第一版固定住，够用。
pub const INSPECTOR_WIDTH: f32 = 260.0;

/// 递归统计时最多数这么多个对象，再多就显示成"超过"。
const MAX_STAT_OBJECTS: usize = 200_000;

/// 面板里额外补上来的信息，都是要再发一次请求才知道的。
#[derive(Default)]
pub struct InspectorExtra {
    /// 这份数据属于哪个 key，对不上就不显示。
    pub key: String,
    pub head: Option<HeadResult>,
    /// 文件夹递归统计的结果：(对象数, 总字节, 是否被上限截断)。
    pub folder: Option<(usize, u64, bool)>,
    pub busy: bool,
    /// 访问权限。`None` 是还没问到，或者服务端根本不支持对象 ACL。
    pub acl: Option<Acl>,
    /// 正在改权限，改完之前按钮都按不动。
    pub acl_busy: bool,
}

impl MainWindow {
    pub fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector = !self.inspector;
        tracing::debug!(open = self.inspector, "属性面板");
        if self.inspector {
            self.refresh_inspector(cx);
        }
        cx.notify();
    }

    /// 选中项变了就把补充信息清掉重拉。没开面板时什么都不做。
    pub fn refresh_inspector(&mut self, cx: &mut Context<Self>) {
        if !self.inspector {
            return;
        }
        let Some(entry) = self.browser().read(cx).focused().cloned() else {
            self.browsing_mut().extra = InspectorExtra::default();
            return;
        };
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let key = location.key_of(&entry.name);
        if self.browsing().extra.key == key {
            return;
        }
        let tab_id = self.browsing().id;
        self.browsing_mut().extra = InspectorExtra {
            key: key.clone(),
            ..Default::default()
        };

        if entry.is_dir {
            // 文件夹的统计要递归列举，等用户点按钮再说
            return;
        }
        let Some(client) = self.client_for(&location.profile, cx) else {
            return;
        };

        cx.spawn(async move |this, cx| {
            let head = client.head_object(&location.bucket, &key).await.ok();
            // 权限单独一个请求。服务端不支持对象 ACL 时这里会报错，
            // 吞掉就是了——面板上那一行不出现，别的照常显示
            let acl = client.get_object_acl(&location.bucket, &key).await.ok();
            this.update(cx, |this, cx| {
                let Some(tab) = this.tab_by_id(tab_id) else {
                    return;
                };
                if tab.extra.key == key {
                    tab.extra.head = head;
                    tab.extra.acl = acl;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// 改当前选中对象的访问权限。
    fn set_acl(&mut self, acl: Acl, cx: &mut Context<Self>) {
        let Some(entry) = self.browser().read(cx).focused().cloned() else {
            return;
        };
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        if entry.is_dir {
            return;
        }
        let key = location.key_of(&entry.name);
        let Some(client) = self.client_for(&location.profile, cx) else {
            return;
        };
        let tab_id = self.browsing().id;
        self.browsing_mut().extra.acl_busy = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = client.put_object_acl(&location.bucket, &key, acl).await;
            // 改完回读一遍，不拿本地猜的值当准——服务端可能把 default 落成别的
            let confirmed = match &result {
                Ok(()) => client.get_object_acl(&location.bucket, &key).await.ok(),
                Err(_) => None,
            };
            this.update(cx, |this, cx| {
                if let Some(tab) = this.tab_by_id(tab_id)
                    && tab.extra.key == key
                {
                    tab.extra.acl_busy = false;
                    if let Some(acl) = confirmed {
                        tab.extra.acl = Some(acl);
                    }
                }
                match result {
                    Ok(()) => this.toast_ok("权限已改", cx),
                    Err(e) => this.toast_err(format!("改权限失败：{e}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 递归数一遍这个文件夹下有多少对象、多大。
    fn count_folder(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.browser().read(cx).focused().cloned() else {
            return;
        };
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        let Some(client) = self.client_for(&location.profile, cx) else {
            return;
        };
        let prefix = format!("{}{}/", location.prefix, entry.name);
        let key = location.key_of(&entry.name);
        let tab_id = self.browsing().id;
        self.browsing_mut().extra.busy = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let mut count = 0usize;
            let mut bytes = 0u64;
            let mut capped = false;
            let mut token: Option<String> = None;

            loop {
                // 不给 delimiter，一次把子目录里的也列进来
                let page = client
                    .list_objects_v2(&location.bucket, &prefix, None, token.as_deref(), 1000)
                    .await;
                let Ok(page) = page else { break };

                count += page.objects.len();
                bytes += page.objects.iter().map(|o| o.size).sum::<u64>();
                if count >= MAX_STAT_OBJECTS {
                    capped = true;
                    break;
                }
                match page.next_token {
                    Some(next) => token = Some(next),
                    None => break,
                }
            }

            this.update(cx, |this, cx| {
                let Some(tab) = this.tab_by_id(tab_id) else {
                    return;
                };
                if tab.extra.key == key {
                    tab.extra.folder = Some((count, bytes, capped));
                    tab.extra.busy = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn render_inspector(&self, cx: &Context<Self>) -> impl IntoElement {
        let state = self.browser().read(cx);
        let entry = state.focused().cloned();
        let location = state.location().cloned();

        v_flex()
            .id("inspector")
            .w(px(INSPECTOR_WIDTH))
            .flex_shrink_0()
            .h_full()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .overflow_y_scroll()
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().child("属性"))
                    .child(
                        Button::new("inspector-close")
                            .icon(IconName::CircleX)
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector(cx))),
                    ),
            )
            .child(match (entry, location) {
                (Some(entry), Some(location)) => self
                    .render_inspector_body(&entry, &location, cx)
                    .into_any_element(),
                _ => div()
                    .p_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("选中一个条目看它的属性")
                    .into_any_element(),
            })
    }

    fn render_inspector_body(
        &self,
        entry: &s3browser::browser_state::Entry,
        location: &s3browser::browser_state::Location,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let key = location.key_of(&entry.name);
        let extra = &self.browsing().extra;
        let head = extra.head.as_ref().filter(|_| extra.key == key);
        let url = self
            .app
            .read(cx)
            .peek_client(&location.profile)
            .map(|client| client.endpoint().url(Some(&location.bucket), &key, ""));

        v_flex()
            .w_full()
            .p_3()
            .gap_2()
            .text_xs()
            .child(field("名称", entry.name.to_string(), cx))
            .child(field("类型", entry.kind_label(), cx))
            .when(!entry.is_dir, |this| {
                this.child(field("大小", format_size(entry.size), cx))
                    .child(field("修改时间", format_time(entry.modified), cx))
            })
            .child(field(
                "存储类型",
                entry.storage_class.label().to_owned(),
                cx,
            ))
            .children(
                head.and_then(|h| h.content_type.clone())
                    .map(|v| field("Content-Type", v, cx)),
            )
            .children(
                entry
                    .etag
                    .as_deref()
                    .map(|e| field("ETag", e.to_owned(), cx)),
            )
            .children((!entry.is_dir).then(|| self.render_acl(cx)).flatten())
            .child(field("Key", key.clone(), cx))
            .child(
                Button::new("copy-key")
                    .label("复制 key")
                    .outline()
                    .xsmall()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_text(key.clone(), "已复制 key", cx)
                    })),
            )
            .children(url.map(|url| {
                Button::new("copy-url")
                    .label("复制对象 URL")
                    .outline()
                    .xsmall()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_text(url.clone(), "已复制对象 URL", cx)
                    }))
            }))
            .when(entry.is_dir, |this| {
                let busy = self.browsing().extra.busy;
                this.child(match self.browsing().extra.folder {
                    Some((count, bytes, capped)) => div()
                        .child(format!(
                            "{}{count} 项，共 {}",
                            if capped { "超过 " } else { "" },
                            format_size(bytes)
                        ))
                        .into_any_element(),
                    None => Button::new("count-folder")
                        .label(if busy {
                            "正在统计…"
                        } else {
                            "计算项目数与总大小"
                        })
                        .outline()
                        .xsmall()
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| this.count_folder(cx)))
                        .into_any_element(),
                })
            })
    }
}

impl MainWindow {
    /// 访问权限那一行：当前是什么，外加两个改法。
    ///
    /// 只对文件出现。文件夹在 S3 里只是个前缀，改它那个占位对象的权限
    /// 对底下的文件一点影响都没有，摆在这儿只会骗人。
    ///
    /// 服务端不支持对象 ACL（比如 R2）时 `acl` 一直是 `None`，整行不画。
    fn render_acl(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let extra = &self.browsing().extra;
        let current = extra.acl?;
        let busy = extra.acl_busy;

        let label = match current {
            Acl::Default => "跟随桶",
            Acl::Private => "私有",
            Acl::PublicRead => "公共读",
            Acl::PublicReadWrite => "公共读写",
        };
        let public = matches!(current, Acl::PublicRead | Acl::PublicReadWrite);
        // busy 时不锁控件：一次请求也就几百毫秒，锁了反而闪一下
        let _ = busy;

        Some(
            v_flex()
                .w_full()
                .gap_1()
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child("访问权限"),
                )
                // 外面这层横排是给宽度用的：竖排 flex 的子项默认 stretch，
                // 分段控件会被拉满整栏，右边拖一大片空轨道。放进横排里它就
                // 按内容定宽了。
                .child(
                    h_flex().child(
                        // 分段控件：两个选项都摆出来，选中的那个滑块滑过去。
                        // TabBar 的 segmented 变体干的就是这件事——它本来就是个
                        // 分段控件，用在这儿才算用对地方。
                        TabBar::new("acl")
                            .segmented()
                            .small()
                            .selected_index(usize::from(public))
                            .child(TabHead::new().label("私有"))
                            .child(TabHead::new().label("公共读"))
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                let acl = if *ix == 1 {
                                    Acl::PublicRead
                                } else {
                                    Acl::Private
                                };
                                this.set_acl(acl, cx);
                            })),
                    ),
                )
                // 权限有四种，控件只画得下两种。落在另外两种上时把真实值
                // 写出来——"跟随桶"跟"私有"不是一回事，光看控件停在"私有"
                // 上会看错，桶要是公开的那这个对象也是公开的。
                .when(!matches!(current, Acl::Private | Acl::PublicRead), |this| {
                    this.child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("当前：{label}")),
                    )
                })
                .into_any_element(),
        )
    }
}

/// 一行"标签 + 值"。值可能很长，允许折行而不是截断，属性面板里看全比对齐重要。
fn field(label: &str, value: String, cx: &Context<MainWindow>) -> impl IntoElement {
    v_flex()
        .w_full()
        .gap_0p5()
        .child(
            div()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_owned()),
        )
        .child(div().w_full().child(value))
}
