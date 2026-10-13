//! 工具栏：前进后退、上级、面包屑、视图切换、刷新、搜索。

use gpui_kit::component::{breadcrumb::*, button::*, input::*, *};
use gpui_kit::*;

use crate::views::main_window::MainWindow;
use s3browser::browser_state::ViewMode;

impl MainWindow {
    /// 工具栏：导航、视图切换、搜索。路径单独占一行，免得长路径挤压这些控件。
    pub fn render_toolbar(&self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 站在传输或设置格上时这些按钮一个都不该能按：它们作用在后台那个桶上，
        // 看不见的东西被改了比按钮灰着难受多了。
        let on_files = self.on_browser_tab();
        let state = self.browser().read(cx);
        let can_back = on_files && state.can_go_back();
        let can_forward = on_files && state.can_go_forward();
        let can_up = on_files && state.can_go_up();
        let has_location = on_files && state.location().is_some();
        let favorited = on_files && state.location().is_some_and(|l| self.is_favorited(l, cx));
        let has_files = on_files && state.selected_entries().iter().any(|e| !e.is_dir);
        let view = state.view();

        h_flex()
            .w_full()
            .h(px(42.))
            .flex_shrink_0()
            .px_2()
            .gap_3()
            .items_center()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().title_bar)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        Button::new("back")
                            .icon(IconName::ArrowLeft)
                            .ghost()
                            .small()
                            .disabled(!can_back)
                            .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
                    )
                    .child(
                        Button::new("forward")
                            .icon(IconName::ArrowRight)
                            .ghost()
                            .small()
                            .disabled(!can_forward)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.go_forward(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("up")
                            .icon(IconName::ArrowUp)
                            .ghost()
                            .small()
                            .disabled(!can_up)
                            .on_click(cx.listener(|this, _, window, cx| this.go_up(window, cx))),
                    ),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        Button::new("upload")
                            .icon(IconName::ArrowUp)
                            .label("上传")
                            .ghost()
                            .small()
                            .disabled(!has_location)
                            .on_click(cx.listener(|this, _, _, cx| this.upload_files(cx))),
                    )
                    .child(
                        Button::new("new-folder")
                            .icon(IconName::FolderClosed)
                            .label("新建文件夹")
                            .ghost()
                            .small()
                            .disabled(!has_location)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_folder(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("download")
                            .icon(IconName::ArrowDown)
                            .label("下载")
                            .ghost()
                            .small()
                            .disabled(!has_files)
                            .on_click(cx.listener(|this, _, _, cx| this.download_selected(cx))),
                    ),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        Button::new("view-list")
                            .icon(IconName::Menu)
                            .ghost()
                            .small()
                            .selected(view == ViewMode::List)
                            .disabled(!on_files)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.set_view(ViewMode::List, cx)),
                            ),
                    )
                    .child(
                        Button::new("view-icon")
                            .icon(IconName::LayoutDashboard)
                            .ghost()
                            .small()
                            .selected(view == ViewMode::Icon)
                            .disabled(!on_files)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.set_view(ViewMode::Icon, cx)),
                            ),
                    )
                    .child(
                        Button::new("refresh")
                            .icon(IconName::RotateCw)
                            .ghost()
                            .small()
                            .disabled(!has_location)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        // 属性面板本来只有 Ctrl+I 能开，没人找得到，给个按钮
                        Button::new("inspector")
                            .icon(IconName::Inbox)
                            .label("属性")
                            .ghost()
                            .small()
                            .selected(self.inspector)
                            .disabled(!has_location)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector(cx))),
                    )
                    .child(
                        // 收藏「你现在站的地方」：桶里某层目录只有从这儿收藏最顺手
                        Button::new("favorite")
                            .icon(IconName::Star)
                            .ghost()
                            .small()
                            .selected(favorited)
                            .disabled(!has_location)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_favorite_here(cx))),
                    ),
            )
            .child(div().flex_1().min_w_0())
            .child(
                div()
                    .w(px(220.))
                    .flex_shrink_0()
                    .child(Input::new(&self.search()).small().disabled(!on_files)),
            )
    }

    /// 地址栏：当前路径，点任一段跳过去。
    pub fn render_path_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .h(px(30.))
            .px_3()
            .items_center()
            .flex_shrink_0()
            .overflow_hidden()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(self.render_breadcrumb(cx))
    }

    fn render_breadcrumb(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("未选择桶")
                .into_any_element();
        };

        let items: Vec<BreadcrumbItem> = location
            .segments()
            .iter()
            .enumerate()
            .map(|(depth, name)| {
                let target = location.truncated(depth);
                BreadcrumbItem::new(name.to_string()).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.open_location(target.clone(), window, cx);
                    },
                ))
            })
            .collect();

        Breadcrumb::new().children(items).into_any_element()
    }

    pub fn set_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        tracing::debug!(?view, "切换视图");
        self.browser().update(cx, |state, cx| {
            state.set_view(view);
            cx.notify();
        });
        self.app.update(cx, |app, _| {
            app.config.ui.view = view;
            app.save_config();
        });
        cx.notify();
    }
}
