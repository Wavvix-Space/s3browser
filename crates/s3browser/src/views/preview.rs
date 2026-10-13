//! 快速预览浮层。Space 打开，Esc 或再按一次 Space 关掉，左右箭头换文件。
//!
//! 内容下载到内存，同时只保留一份：切到下一个文件时上一份就被顶掉了，关掉浮层则整个释放。

use std::sync::Arc;

use gpui_kit::component::{button::*, *};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use s3browser::browser_state::format_size;
use s3browser::preview::{self, ImageKind, PreviewKind};

use crate::views::main_window::MainWindow;

/// 浮层正在显示什么。
pub enum PreviewContent {
    Loading,
    Image(Arc<Image>),
    Text {
        body: SharedString,
        /// 太长截过一刀，底下提一句。
        clipped: bool,
    },
    /// 没有内容可画，只给一句说明。
    Note(SharedString),
}

pub struct Preview {
    pub name: String,
    pub size: u64,
    pub modified: i64,
    pub content: PreviewContent,
    /// 这次加载的序号。切了文件之后旧的响应回来对不上号就丢掉。
    token: u64,
}

impl MainWindow {
    /// Space 的行为：开着就关，关着就对当前项打开。
    pub fn toggle_preview(&mut self, cx: &mut Context<Self>) {
        if self.preview.is_some() {
            self.close_preview(cx);
        } else {
            self.open_preview(cx);
        }
    }

    pub fn close_preview(&mut self, cx: &mut Context<Self>) {
        if self.preview.take().is_some() {
            cx.notify();
        }
    }

    /// 对当前项打开预览。没有当前项（没选或多选）就不开。
    pub fn open_preview(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.browser().read(cx).focused().cloned() else {
            self.toast_warn("先选一个文件再按空格", cx);
            return;
        };
        if entry.is_dir {
            return;
        }

        let kind = preview::classify(&entry.name, entry.size, entry.is_dir);
        tracing::debug!(name = %entry.name, size = entry.size, ?kind, "打开预览");
        self.preview_token += 1;
        let token = self.preview_token;

        self.preview = Some(Preview {
            name: entry.name.to_string(),
            size: entry.size,
            modified: entry.modified,
            content: match &kind {
                PreviewKind::TooLarge(why) => PreviewContent::Note((*why).into()),
                PreviewKind::Unsupported => PreviewContent::Note("这个类型不支持预览".into()),
                _ => PreviewContent::Loading,
            },
            token,
        });
        cx.notify();

        if kind.needs_body() {
            self.fetch_preview(entry.name.to_string(), entry.size, kind, token, cx);
        }
    }

    /// 预览里按左右箭头：把选中项挪一格，再对新的那个打开。
    pub fn step_preview(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(row) = self.browser().read(cx).focused_row() else {
            return;
        };
        let next = (row as isize + delta).max(0) as usize;
        let moved = self.browser().update(cx, |state, cx| {
            let moved = state.focus_row(next).is_some();
            if moved {
                cx.notify();
            }
            moved
        });
        if moved {
            self.refresh_table(cx);
            self.open_preview(cx);
        }
    }

    /// 取内容。只读到上限为止，超了就当截断处理，不会把大对象整个吞进内存。
    fn fetch_preview(
        &mut self,
        name: String,
        size: u64,
        kind: PreviewKind,
        token: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let Some(client) = self.client_for(&location.profile, cx) else {
            return;
        };
        let key = location.key_of(&name);
        let cap = match kind {
            PreviewKind::Image(_) => preview::MAX_IMAGE_BYTES,
            _ => preview::MAX_TEXT_BYTES,
        };

        cx.spawn(async move |this, cx| {
            let result = read_capped(&client, &location.bucket, &key, cap).await;

            this.update(cx, |this, cx| {
                // 已经翻到下一个文件或者关掉浮层了，这份内容不要了
                let Some(current) = this.preview.as_mut() else {
                    return;
                };
                if current.token != token {
                    return;
                }
                current.content = match result {
                    Err(e) => PreviewContent::Note(format!("读不到内容：{e}").into()),
                    Ok(bytes) => match kind {
                        PreviewKind::Image(image_kind) => PreviewContent::Image(Arc::new(
                            Image::from_bytes(image_format(image_kind), bytes),
                        )),
                        _ => {
                            // 下载本身就可能被上限截断，这时也算截过
                            let truncated = (bytes.len() as u64) < size;
                            let text = preview::decode_text(&bytes);
                            let (body, clipped) = preview::clip_text(&text);
                            PreviewContent::Text {
                                body: body.into(),
                                clipped: clipped || truncated,
                            }
                        }
                    },
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------- 渲染 ----------

    pub fn render_preview(&self, preview: &Preview, cx: &Context<Self>) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.55))
            .id("preview-backdrop")
            // 点背景关掉，和 Finder 一样
            .on_click(cx.listener(|this, _, _, cx| this.close_preview(cx)))
            .child(
                v_flex()
                    .id("preview-panel")
                    // 跟着窗口走。原来是固定 780 宽 + max_h 620——max_h 只封顶不
                    // 撑高，内容短的时候面板自己缩成扁扁一条，图片也就只能挤在
                    // 中间那么点地方。外面那层正好铺满窗口，所以这里的百分比
                    // 就是窗口的百分比。
                    .w(relative(0.72))
                    .h(relative(0.82))
                    // 大屏上别铺太满，看图还是要留点边
                    .max_w(px(1400.))
                    .max_h(px(1100.))
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .overflow_hidden()
                    // 面板里的点击不该穿透到背景把自己关掉
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(self.render_preview_header(preview, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(200.))
                            .p_3()
                            .overflow_hidden()
                            .child(self.render_preview_body(preview, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_preview_header(&self, preview: &Preview, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().title_bar)
            .child(
                v_flex()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(preview.name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{} · {}",
                                format_size(preview.size),
                                crate::views::list_view::format_time(preview.modified)
                            )),
                    ),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        Button::new("preview-prev")
                            .icon(IconName::ArrowLeft)
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.step_preview(-1, cx))),
                    )
                    .child(
                        Button::new("preview-next")
                            .icon(IconName::ArrowRight)
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.step_preview(1, cx))),
                    )
                    .child(
                        Button::new("preview-close")
                            .icon(IconName::CircleX)
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.close_preview(cx))),
                    ),
            )
    }

    fn render_preview_body(&self, preview: &Preview, cx: &Context<Self>) -> AnyElement {
        match &preview.content {
            PreviewContent::Loading => center_note("正在读取…", cx).into_any_element(),
            PreviewContent::Note(message) => center_note(message.clone(), cx).into_any_element(),
            PreviewContent::Image(image) => img(image.clone())
                .max_w_full()
                .max_h(px(520.))
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            PreviewContent::Text { body, clipped } => v_flex()
                .size_full()
                .gap_2()
                .child(
                    div()
                        .id("preview-text")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .font_family("monospace")
                        .text_xs()
                        .child(body.clone()),
                )
                .when(*clipped, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("内容太长，只显示了开头一段"),
                    )
                })
                .into_any_element(),
        }
    }
}

fn center_note(message: impl Into<SharedString>, cx: &Context<MainWindow>) -> Div {
    h_flex()
        .size_full()
        .items_center()
        .justify_center()
        .text_color(cx.theme().muted_foreground)
        .child(message.into())
}

fn image_format(kind: ImageKind) -> ImageFormat {
    match kind {
        ImageKind::Png => ImageFormat::Png,
        ImageKind::Jpeg => ImageFormat::Jpeg,
        ImageKind::Gif => ImageFormat::Gif,
        ImageKind::Webp => ImageFormat::Webp,
        ImageKind::Bmp => ImageFormat::Bmp,
        ImageKind::Svg => ImageFormat::Svg,
    }
}

/// 读响应体，读满 `cap` 就停。预览不需要完整对象。
pub(crate) async fn read_capped(
    client: &s3api::S3Client,
    bucket: &str,
    key: &str,
    cap: u64,
) -> Result<Vec<u8>, s3api::S3Error> {
    use futures_util::StreamExt;

    let result = client.get_object(bucket, key, None, None).await?;
    let mut stream = result.body;
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| s3api::S3Error::Transport(s3api::TransportError::Io(e.to_string())))?;
        out.extend_from_slice(&chunk);
        if out.len() as u64 >= cap {
            out.truncate(cap as usize);
            break;
        }
    }
    Ok(out)
}
