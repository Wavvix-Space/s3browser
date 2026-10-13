//! 列表视图。Table 自带虚拟滚动，十万行也只渲染可见的那些。

use gpui_kit::component::{table::*, *};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::views::drag::draggable;
use crate::views::file_icon;
use crate::views::main_window::MainWindow;
use s3browser::browser_state::{BrowserState, SortColumn, format_size};

pub struct FileTable {
    browser: Entity<BrowserState>,
    main: WeakEntity<MainWindow>,
    columns: Vec<Column>,
}

impl FileTable {
    pub fn new(browser: Entity<BrowserState>, main: WeakEntity<MainWindow>) -> Self {
        FileTable {
            browser,
            main,
            columns: vec![
                Column::new("name", "名称")
                    .width(px(360.))
                    .min_width(px(120.))
                    .sortable()
                    .ascending()
                    .resizable(true),
                Column::new("modified", "修改日期")
                    .width(px(170.))
                    .sortable()
                    .resizable(true),
                Column::new("size", "大小")
                    .width(px(110.))
                    .text_right()
                    .sortable()
                    .resizable(true),
                Column::new("kind", "类型")
                    .width(px(130.))
                    .sortable()
                    .resizable(true),
            ],
        }
    }

    fn sort_column(col_ix: usize) -> SortColumn {
        match col_ix {
            0 => SortColumn::Name,
            1 => SortColumn::Modified,
            2 => SortColumn::Size,
            _ => SortColumn::Kind,
        }
    }
}

impl TableDelegate for FileTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, cx: &App) -> usize {
        self.browser.read(cx).visible_count()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let descending = matches!(sort, ColumnSort::Descending);
        for (ix, column) in self.columns.iter_mut().enumerate() {
            column.sort = if ix == col_ix { Some(sort) } else { None };
        }
        self.browser.update(cx, |state, _| {
            state.set_sort(Self::sort_column(col_ix), descending);
        });
        cx.notify();
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let state = self.browser.read(cx);
        let selected = state
            .row_index(row_ix)
            .is_some_and(|ix| state.is_selected(ix));
        let selected_count = state.selected_count();
        let entry = state.row(row_ix).cloned();
        let main = self.main.clone();

        let row = div()
            .id(("row", row_ix))
            .when(selected, |this| {
                this.bg(cx.theme().accent)
                    .text_color(cx.theme().accent_foreground)
            })
            // 右键先选中自己再挡住冒泡。菜单挂在内容区那一层，
            // 外层据此判断给条目菜单还是目录菜单。
            //
            // 不走表格 delegate 的 context_menu：那个回调是在 TableState 的
            // update 里跑的，在里面刷新表格等于重入更新同一个实体，GPUI 会 panic。
            .on_mouse_down(gpui_kit::MouseButton::Right, {
                let main = self.main.clone();
                move |_, window, cx| {
                    main.update(cx, |this, cx| {
                        let already = this
                            .browser()
                            .read(cx)
                            .row_index(row_ix)
                            .is_some_and(|ix| this.browser().read(cx).is_selected(ix));
                        if !already {
                            this.on_row_click(row_ix, false, false, false, window, cx);
                        }
                    })
                    .ok();
                    cx.stop_propagation();
                }
            })
            .on_click(move |event, window, cx| {
                let modifiers = event.modifiers();
                let double = click_count(event) >= 2;
                main.update(cx, |this, cx| {
                    this.on_row_click(
                        row_ix,
                        modifiers.secondary(),
                        modifiers.shift,
                        double,
                        window,
                        cx,
                    );
                })
                .ok();
            });

        // 文件夹行既能被拖走，也能接住拖过来的东西
        match entry {
            Some(entry) => draggable(
                row,
                self.main.clone(),
                &entry.name,
                entry.is_dir,
                selected_count,
            ),
            None => row,
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(entry) = self.browser.read(cx).row(row_ix).cloned() else {
            return div().into_any_element();
        };
        match col_ix {
            0 => h_flex()
                .gap_2()
                .items_center()
                .child(file_icon::for_entry(&entry, cx).size_4())
                .child(entry.name.to_string())
                .into_any_element(),
            1 => format_time(entry.modified).into_any_element(),
            2 => if entry.is_dir {
                "--".to_owned()
            } else {
                format_size(entry.size)
            }
            .into_any_element(),
            _ => entry.kind_label().into_any_element(),
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let state = self.browser.read(cx);
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

    /// 目录很大时先显示第一页，剩下的在后台继续拉。
    fn has_more(&self, cx: &App) -> bool {
        self.browser.read(cx).next_token().is_some()
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        let main = self.main.clone();
        main.update(cx, |this, cx| this.load_next_page(cx)).ok();
    }
}

fn click_count(event: &ClickEvent) -> usize {
    match event {
        ClickEvent::Mouse(event) => event.down.click_count,
        _ => 1,
    }
}

/// 本地时区的 `2026-09-20 14:05`。文件夹没有时间，显示横杠。
pub fn format_time(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "--".to_owned();
    }
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "--".to_owned())
}
