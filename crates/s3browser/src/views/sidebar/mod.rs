//! 侧边栏：上面是收藏，下面是全部账户，底部是添加账户、传输和设置。
//!
//! 没用 `Sidebar` 容器组件：它内部拿虚拟列表渲染内容，账户就那么几个，
//! 自己排版更简单，也好在底部固定放几个入口。
//!
//! 账户行和桶行也是自己画的，不用 `SidebarMenuItem`：那个组件内部还套了一层
//! 带 id 的 `#item`，鼠标按下会被它的 hitbox 收走。账户要能拖着排序，拖拽就得
//! 和点击落在同一个元素上——在外面包一层挂 `on_drag` 永远等不到拖拽开始。
//!
//! 拆成几个文件是因为这儿要管的事不止渲染：桶列表要拉、账户要增删改、收藏要增删、
//! 三个域要能拖着排序。按这几件事分家，别按"组件"分。

mod accounts;
mod buckets;
pub(crate) mod dnd;
mod favorites;
mod rows;

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::views::main_window::MainWindow;
use dnd::{DraggedFavorite, DraggedProfile, DropDomain};

/// 账户行和桶行的高度。两者一样高，缩进不同。
pub(crate) const ROW_HEIGHT: f32 = 28.0;

/// 侧边栏上的两个分组。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarGroup {
    Favorites,
    All,
}

impl MainWindow {
    pub fn render_sidebar(&self, width: Pixels, cx: &mut Context<Self>) -> impl IntoElement {
        let (ui, profiles, favorite_count) = {
            let app = self.app.read(cx);
            (
                app.config.ui.clone(),
                app.config.profiles.clone(),
                app.config.favorites.len(),
            )
        };
        let current = self.browser().read(cx).location().cloned();

        let favorite_rows = self.render_favorites(current.as_ref(), cx);
        let account_rows: Vec<AnyElement> = if ui.all_collapsed {
            Vec::new()
        } else {
            profiles
                .iter()
                .enumerate()
                .map(|(ix, profile)| {
                    self.render_account(ix, profiles.len(), profile, current.as_ref(), cx)
                })
                .collect()
        };

        v_flex()
            .w(width)
            .h_full()
            .flex_shrink_0()
            .overflow_hidden()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                // 左右的留白给到行自己，不放在这一层：滚动条是贴着滚动区右边缘画的，
                // 这层要是有 px，条就缩进来压在账户行上，看着像长在条目里面。
                //
                // 收藏多了、桶又都展开时一屏装不下，这一块自己滚，底部那三个入口留在原地。
                // 里外两层缺一不可：外层 flex_1 + min_h_0 把高度钉死在剩余空间上，
                // 内层 size_full 才有确定的高度可滚。分组头也在滚动区里，跟着一起滚。
                div().flex_1().min_h_0().py_2().child(
                    v_flex()
                        .size_full()
                        .px_2()
                        .gap_0p5()
                        .overflow_y_scrollbar()
                        .child(
                            group_header(
                                "group-favorites",
                                "收藏",
                                ui.favorites_collapsed,
                                favorite_count,
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_group(SidebarGroup::Favorites, cx)
                            })),
                        )
                        .when(!ui.favorites_collapsed, |this| {
                            this.child(
                                // 收藏区自成一个落点范围：拖出这块线就收起，在这块里松手才算数
                                v_flex()
                                    .children(favorite_rows)
                                    .on_drag_move(cx.listener(
                                        |this, event: &DragMoveEvent<DraggedFavorite>, _, cx| {
                                            this.leave_drop_zone(&DropDomain::Favorites, event, cx);
                                        },
                                    ))
                                    .on_drop(cx.listener(
                                        |this, dragged: &DraggedFavorite, _, cx| {
                                            this.drop_favorite(dragged.index, cx);
                                        },
                                    )),
                            )
                        })
                        // 两组之间留口气，不然分不清哪行归哪组
                        .child(div().h_2().flex_shrink_0())
                        .child(
                            group_header(
                                "group-all",
                                "全部",
                                ui.all_collapsed,
                                profiles.len(),
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_group(SidebarGroup::All, cx)
                            })),
                        )
                        .when(!ui.all_collapsed, |this| {
                            this.child(
                                // 账户列表自成一个落点范围，道理同上
                                v_flex()
                                    .children(account_rows)
                                    .on_drag_move(cx.listener(
                                        |this, event: &DragMoveEvent<DraggedProfile>, _, cx| {
                                            this.leave_drop_zone(&DropDomain::Profiles, event, cx);
                                        },
                                    ))
                                    .on_drop(cx.listener(
                                        |this, dragged: &DraggedProfile, _, cx| {
                                            this.drop_profile(&dragged.id, cx);
                                        },
                                    )),
                            )
                        }),
                ),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        sidebar_action("add-account", IconName::Plus, "添加账户", cx).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.open_account_dialog(None, window, cx);
                            }),
                        ),
                    )
                    .child({
                        let running = self.queue.read(cx).active_count();
                        let label = if running > 0 {
                            format!("传输 ({running})")
                        } else {
                            "传输".to_owned()
                        };
                        sidebar_action_labeled("transfers", IconName::ArrowUp, label, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.show_transfers(cx)))
                    })
                    .child(
                        sidebar_action("settings", IconName::Settings, "设置", cx).on_click(
                            cx.listener(|this, _, window, cx| this.show_settings(window, cx)),
                        ),
                    ),
            )
    }

    /// 收起或展开一个分组。状态落盘，下次打开还是这样。
    pub fn toggle_group(&mut self, group: SidebarGroup, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            let ui = &mut app.config.ui;
            match group {
                SidebarGroup::Favorites => ui.favorites_collapsed = !ui.favorites_collapsed,
                SidebarGroup::All => ui.all_collapsed = !ui.all_collapsed,
            }
            app.save_config();
            cx.notify();
        });
        cx.notify();
    }
}

/// 分组头：一段列表上方那行小字，点一下收起整组。
///
/// 比账户行更矮更淡，是为了让人一眼看出它不是一个条目——
/// 长得一样的话，点它收起整组会像误触。
fn group_header(
    id: &'static str,
    label: &'static str,
    collapsed: bool,
    count: usize,
    cx: &Context<MainWindow>,
) -> Stateful<Div> {
    let theme = cx.theme();
    h_flex()
        .id(id)
        .w_full()
        .h(px(24.))
        .flex_shrink_0()
        .px_2()
        .gap_1()
        .items_center()
        .text_xs()
        .rounded(theme.radius)
        .cursor_pointer()
        .text_color(theme.sidebar_foreground.opacity(0.7))
        .hover(|this| this.bg(theme.sidebar_accent.opacity(0.5)))
        .child(
            Icon::new(IconName::ChevronRight)
                .size_3p5()
                .flex_shrink_0()
                .when(!collapsed, |this| this.rotate(percentage(90. / 360.))),
        )
        .child(div().flex_1().min_w_0().truncate().child(label))
        // 收起来的时候给个数，不然不知道里面还有没有东西
        .when(collapsed && count > 0, |this| {
            this.child(
                div()
                    .flex_shrink_0()
                    .text_color(theme.muted_foreground)
                    .child(count.to_string()),
            )
        })
}

fn sidebar_action(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    cx: &Context<MainWindow>,
) -> Stateful<Div> {
    sidebar_action_labeled(id, icon, label.to_owned(), cx)
}

/// 标题要带个数（「传输 (3)」）时用这个。
fn sidebar_action_labeled(
    id: &'static str,
    icon: IconName,
    label: String,
    cx: &Context<MainWindow>,
) -> Stateful<Div> {
    h_flex()
        .id(id)
        .w_full()
        .gap_2()
        .items_center()
        .px_2()
        .py_1()
        .text_sm()
        .rounded(cx.theme().radius)
        .cursor_pointer()
        .hover(|this| this.bg(cx.theme().sidebar_accent.opacity(0.8)))
        .child(Icon::new(icon).size_4())
        .child(label)
}

/// 列表里占一行的灰字提示：没有收藏、桶还在加载、桶都藏起来了之类。
fn hint_row(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    h_flex()
        .h(px(ROW_HEIGHT))
        .px_2()
        .flex_shrink_0()
        .items_center()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(div().min_w_0().truncate().child(text.into()))
        .into_any_element()
}
