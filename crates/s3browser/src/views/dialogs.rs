//! 通用的输入框和确认框。样式跟账户对话框一致，都是自绘浮层。

use gpui_kit::component::input::InputEvent;
use gpui_kit::component::{button::*, input::*, *};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::views::main_window::MainWindow;
use s3browser::browser_state::{Item, Location};
use s3browser::ops::validate_name;

/// 输入框要做什么。确定之后由主窗口按这个分派。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPurpose {
    NewFolder,
    /// 重命名，带上原来的完整 key。
    Rename {
        key: String,
        is_dir: bool,
    },
    /// 前往路径。输入的是相对当前桶的前缀，可以带斜杠。
    GoTo,
    /// 给收藏项改名。`index` 是它在 `config.favorites` 里的位置。
    /// 收藏名是给人看的，带斜杠无所谓，所以不走 `strict_name`；空名照样挡下。
    RenameFavorite { index: usize },
}

impl InputPurpose {
    /// 要不要按"单个条目名"校验：不能带斜杠，不能是 `.` 或 `..`。
    /// 前往路径本来就是多段路径，不能这么管。
    fn strict_name(&self) -> bool {
        matches!(self, InputPurpose::NewFolder | InputPurpose::Rename { .. })
    }
}

pub struct InputDialog {
    main: WeakEntity<MainWindow>,
    purpose: InputPurpose,
    title: SharedString,
    /// 标题底下的一句说明，没有就不显示。
    hint: Option<SharedString>,
    input: Entity<InputState>,
    error: Option<SharedString>,
}

impl InputDialog {
    pub fn new(
        purpose: InputPurpose,
        title: impl Into<SharedString>,
        initial: &str,
        hint: Option<SharedString>,
        placeholder: &'static str,
        main: WeakEntity<MainWindow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial = initial.to_owned();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(initial)
        });
        // 敲完回车就该生效，不然这种小对话框每次都得摸一下鼠标
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.confirm(window, cx);
            }
        })
        .detach();
        // 打开就把光标放进去
        input.update(cx, |state, cx| state.focus(window, cx));

        InputDialog {
            main,
            purpose,
            title: title.into(),
            hint,
            input,
            error: None,
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.input.read(cx).value().trim().to_string();
        if self.purpose.strict_name()
            && let Err(message) = validate_name(&name)
        {
            self.error = Some(message.into());
            cx.notify();
            return;
        }
        if name.is_empty() {
            self.error = Some("名称不能为空".into());
            cx.notify();
            return;
        }
        let purpose = self.purpose.clone();
        if let Some(main) = self.main.upgrade() {
            main.update(cx, |main, cx| {
                main.dialog = None;
                main.on_input_confirmed(purpose, name, window, cx);
            });
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(main) = self.main.upgrade() {
            main.update(cx, |main, cx| {
                main.dialog = None;
                cx.notify();
            });
        }
    }
}

impl Render for InputDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        overlay(cx).child(
            v_flex()
                .w(px(400.))
                .p_4()
                .gap_3()
                .rounded(cx.theme().radius_lg)
                .bg(cx.theme().popover)
                .border_1()
                .border_color(cx.theme().border)
                .child(div().font_bold().child(self.title.clone()))
                .when_some(self.hint.clone(), |this, hint| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(hint),
                    )
                })
                .child(Input::new(&self.input))
                .when_some(self.error.clone(), |this, error| {
                    this.child(div().text_xs().text_color(cx.theme().red).child(error))
                })
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel")
                                .label("取消")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                        )
                        .child(
                            Button::new("ok").label("确定").primary().on_click(
                                cx.listener(|this, _, window, cx| this.confirm(window, cx)),
                            ),
                        ),
                ),
        )
    }
}

/// 确认之后要做什么。
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// 删除这些条目（名字，是否是文件夹）。
    ///
    /// 位置在开框的那一刻就记下来，不到确认时再问"当前在哪"：
    /// 框开着的时候底下完全可能已经换了目录，那时候算出来的 key 是别人的。
    Delete {
        location: Location,
        items: Vec<Item>,
    },
    /// 删掉一个账户。只动本地配置，服务端的东西一个不碰。
    RemoveAccount { id: String, name: String },
}

pub struct ConfirmDialog {
    main: WeakEntity<MainWindow>,
    title: SharedString,
    message: SharedString,
    action: ConfirmAction,
    focus: FocusHandle,
}

impl ConfirmDialog {
    pub fn new(
        action: ConfirmAction,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        main: WeakEntity<MainWindow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 开框就把焦点抢过来，键盘才轮得到下面那个 on_key_down
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        ConfirmDialog {
            main,
            title: title.into(),
            message: message.into(),
            action,
            focus,
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let action = self.action.clone();
        if let Some(main) = self.main.upgrade() {
            main.update(cx, |main, cx| {
                main.dialog = None;
                main.on_confirmed(action, cx);
            });
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(main) = self.main.upgrade() {
            main.update(cx, |main, cx| {
                main.dialog = None;
                cx.notify();
            });
        }
    }
}

impl Render for ConfirmDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        overlay(cx)
            .track_focus(&self.focus)
            // 框开着的时候按键不能漏到底下的列表去。之前 Enter 会落到文件列表上
            // 触发"打开"，把身后的目录切走，然后这个框按新位置去删——删的就不是
            // 当初选的那批东西了。
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                cx.stop_propagation();
                match event.keystroke.key.as_str() {
                    "enter" => this.confirm(cx),
                    "escape" => this.cancel(cx),
                    _ => {}
                }
            }))
            .child(
                v_flex()
                    .w(px(400.))
                    .p_4()
                    .gap_3()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(div().font_bold().child(self.title.clone()))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.message.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("cancel")
                                    .label("取消")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                            )
                            .child(
                                Button::new("ok")
                                    .label("删除")
                                    .danger()
                                    .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                            ),
                    ),
            )
    }
}

fn overlay(_cx: &App) -> Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::black().opacity(0.35))
}
