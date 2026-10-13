//! 拖放。
//!
//! 列表的行和图标视图的单元格都挂这一套：拖它走、往它上面放。
//! 落点那边真正干活的是 `MainWindow::drop_entries_into` 和 `drop_into_folder`。

use gpui_kit::component::*;
use gpui_kit::*;

use crate::views::main_window::MainWindow;

/// 应用内拖动时带着的东西。具体拖了哪些条目在落点现取当前选择，
/// 所以这里只带一个数量用来画那个跟着鼠标的小标签。
#[derive(Clone)]
pub struct DraggedEntries {
    pub count: usize,
}

/// 拖动时跟着鼠标走的小标签。
pub struct DragLabel(SharedString);

impl DragLabel {
    pub fn new(label: impl Into<SharedString>) -> Self {
        DragLabel(label.into())
    }
}

impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .text_xs()
            .rounded(cx.theme().radius)
            .bg(cx.theme().accent)
            .text_color(cx.theme().accent_foreground)
            .child(self.0.clone())
    }
}

/// 给一行或一个图标挂上"拖它"和"往它上面放"两件事。
///
/// 拖到文件夹上默认是移动，按住 Ctrl 变复制——和资源管理器一个规矩。
/// 外部文件拖进来则一律是上传。列表视图的行和图标视图的单元格各自拿到的
/// Context 类型不一样，所以这里收一个 `WeakEntity` 而不是 `cx.listener`。
pub fn draggable<E: StatefulInteractiveElement>(
    element: E,
    main: WeakEntity<MainWindow>,
    entry_name: &str,
    is_dir: bool,
    selected: usize,
) -> E {
    // 没选中任何东西就按拖的这一个算
    let count = selected.max(1);
    let element = element.on_drag(DraggedEntries { count }, move |dragged, _, _, cx| {
        let label: SharedString = format!("{} 项", dragged.count).into();
        cx.new(|_| DragLabel(label))
    });

    // 只有文件夹能接东西
    if !is_dir {
        return element;
    }
    let internal = entry_name.to_owned();
    let external = entry_name.to_owned();
    let for_internal = main.clone();

    element
        .drag_over::<DraggedEntries>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.4)))
        .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.4)))
        .on_drop(move |_: &DraggedEntries, window, cx| {
            // 按住 Ctrl 是复制，什么都不按是移动
            let cut = !window.modifiers().secondary();
            let name = internal.clone();
            for_internal
                .update(cx, |this, cx| this.drop_entries_into(name, cut, cx))
                .ok();
        })
        .on_drop(move |paths: &ExternalPaths, _, cx| {
            let paths = paths.paths().to_vec();
            let name = external.clone();
            main.update(cx, |this, cx| this.drop_into_folder(Some(name), paths, cx))
                .ok();
        })
}
