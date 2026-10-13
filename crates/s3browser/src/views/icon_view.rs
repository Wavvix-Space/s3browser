//! 图标视图。虚拟化的粒度是"一行"，一行里塞满一排单元格。
//!
//! 每行列数按内容区宽度除以单元格宽度算出来，所以宽度必须是确定的像素值——
//! 这里不做测量，由 `render` 从窗口宽度减掉侧边栏和属性面板算好传进来。

use std::rc::Rc;

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use s3browser::browser_state::Entry;

use crate::views::drag::draggable;
use crate::views::file_icon;
use crate::views::main_window::MainWindow;

/// 单元格宽度，设计文档里就是按 96 px 除的。
const CELL_WIDTH: f32 = 96.0;
/// 图标边长。
const ICON_SIZE: f32 = 64.0;
/// 高度要够放下图标加两行文件名：上下留白 8+8、图标 64、间距 4、两行 12 px 字约 36。
/// 给少了 flex 会把文件名那块压扁，只露出半行字。
const CELL_HEIGHT: f32 = 124.0;
/// 网格四周的留白，算列数时要先扣掉。
const GRID_PADDING: f32 = 8.0;

/// 给定内容宽度能放下几列。至少一列，再窄也不能算出 0 来除。
pub fn columns_for(width: Pixels) -> usize {
    let usable = f32::from(width) - GRID_PADDING * 2.0;
    ((usable / CELL_WIDTH).floor() as usize).max(1)
}

impl MainWindow {
    /// 图标视图。`content_width` 是内容区的实际宽度。
    pub fn render_icon_view(
        &mut self,
        content_width: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = self.browser().read(cx).visible_count();
        if count == 0 {
            return self.render_empty_hint(cx).into_any_element();
        }

        let cols = columns_for(content_width);
        let rows = count.div_ceil(cols);
        // VirtualList 竖排时只用 height，width 给全宽让行铺满
        let sizes = Rc::new(vec![size(content_width, px(CELL_HEIGHT)); rows]);

        // 一页一个滚动句柄：切标签回来还在原来那个位置，也才画得出滚动条
        let scroll = self.browsing().grid_scroll.clone();
        let grid = v_virtual_list(
            cx.entity(),
            "icon-grid",
            sizes,
            move |this, range, _, cx| {
                // 快滚到底就继续翻页，和列表视图里表格干的是一件事
                if range.end + 2 >= this.browser().read(cx).visible_count().div_ceil(cols) {
                    this.load_next_page(cx);
                }
                range
                    .map(|row| this.render_icon_row(row, cols, cx))
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&scroll)
        .px(px(GRID_PADDING))
        .size_full();

        // VirtualList 自己带滚轮，但不画滚动条——超出一屏之后没有任何东西
        // 告诉你下面还有，也没法拖。条浮在网格上层，所以外面包一层 relative。
        div()
            .relative()
            .size_full()
            .child(grid)
            .vertical_scrollbar(&scroll)
            .into_any_element()
    }

    /// 一整行单元格。最后一行可能不满，缺的位置不补空元素。
    fn render_icon_row(&mut self, row: usize, cols: usize, cx: &mut Context<Self>) -> Div {
        let first = row * cols;
        let cells: Vec<AnyElement> = (first..(first + cols))
            .filter_map(|index| {
                let state = self.browser().read(cx);
                let entry = state.row(index)?.clone();
                let selected = state
                    .row_index(index)
                    .is_some_and(|ix| state.is_selected(ix));
                Some(self.render_icon_cell(index, entry, selected, cx))
            })
            .collect();

        h_flex().h(px(CELL_HEIGHT)).items_start().children(cells)
    }

    fn render_icon_cell(
        &mut self,
        index: usize,
        entry: Entry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_dir = entry.is_dir;
        let name = entry.name.to_string();
        let selected_count = self.browser().read(cx).selected_count();

        let cell = div()
            .id(("cell", index))
            .w(px(CELL_WIDTH))
            .h(px(CELL_HEIGHT))
            .p_1()
            // 右键先把这一格选上，然后挡住冒泡。
            //
            // 菜单本身挂在外层网格上，只挂一处：右键菜单是靠命中框触发的，
            // 单元格和网格的命中框是嵌套的，两边都挂就会弹出两个菜单叠在一起。
            // 挡住冒泡之后，外层那个清空选择的处理器不会跑，菜单就按"有选中项"
            // 给条目菜单；点在空白处才轮到外层清空，给的是目录菜单。
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    let already = this
                        .browser()
                        .read(cx)
                        .row_index(index)
                        .is_some_and(|ix| this.browser().read(cx).is_selected(ix));
                    if !already {
                        this.on_row_click(index, false, false, false, window, cx);
                    }
                    cx.stop_propagation();
                }),
            );

        let cell = draggable(
            cell,
            cx.entity().downgrade(),
            &entry.name,
            is_dir,
            selected_count,
        )
        .child(
            v_flex()
                .size_full()
                .py_2()
                .gap_1()
                .items_center()
                .justify_start()
                .rounded(cx.theme().radius)
                .when(selected, |this| {
                    this.bg(cx.theme().accent)
                        .text_color(cx.theme().accent_foreground)
                })
                // 选中了也保留本色。选中态靠底色和文字颜色表示，图标跟着翻成
                // 前景色的话，选中的那一格反而成了唯一一个黑白的。
                .child(
                    file_icon::for_entry(&entry, cx)
                        .with_size(px(ICON_SIZE))
                        .flex_shrink_0(),
                )
                .child(
                    div()
                        .w_full()
                        .px_1()
                        .text_xs()
                        .text_center()
                        // 一行放不下就从中间省略，保住扩展名——文件浏览器里
                        // 后缀比中间那几个字符有用。
                        //
                        // 不用 line_clamp：它只限行数，不设 text_overflow，
                        // 所以不会出省略号；而且刚好能挤进两行的名字（像
                        // kimi_3.0.26.exe）会断成一行半，最后拖一个字母下来。
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_middle()
                        .child(name),
                ),
        )
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            let modifiers = event.modifiers();
            let double = matches!(event, ClickEvent::Mouse(e) if e.down.click_count >= 2);
            this.on_row_click(
                index,
                modifiers.secondary(),
                modifiers.shift,
                double,
                window,
                cx,
            );
        }));

        cell.into_any_element()
    }

    /// 两个视图共用的空状态文案。
    pub fn render_empty_hint(&self, cx: &Context<Self>) -> Div {
        let state = self.browser().read(cx);
        let message = if let Some(error) = state.error() {
            error.to_owned()
        } else if state.is_loading() {
            "正在加载…".to_owned()
        } else if !state.filter().is_empty() {
            "没有匹配的条目".to_owned()
        } else if state.location().is_some() {
            "这个目录是空的".to_owned()
        } else {
            "从左侧选一个桶".to_owned()
        };

        h_flex()
            .size_full()
            .justify_center()
            .items_center()
            .text_color(cx.theme().muted_foreground)
            .child(message)
    }
}
