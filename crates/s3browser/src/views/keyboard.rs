//! 键盘导航：方向键、全选、几个跳转类的快捷键。
//!
//! 键位表在 `main.rs`，这里只有动作被触发之后干什么。

use gpui_kit::*;

use s3browser::browser_state::ViewMode;
use s3browser::ops;

use crate::views::dialogs::InputPurpose;
use crate::views::main_window::MainWindow;

impl MainWindow {
    /// Enter：文件夹进去，文件开预览。
    pub fn open_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.browser().read(cx).focused().cloned() else {
            return;
        };
        if entry.is_dir {
            let Some(location) = self.browser().read(cx).location().cloned() else {
                return;
            };
            self.open_location(location.child(&entry.name), window, cx);
        } else {
            self.open_preview(cx);
        }
    }

    /// 方向键。`delta` 已经换算成行的增量：列表视图是 ±1，图标视图上下是 ±列数。
    pub fn move_focus(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let moved = self.browser().update(cx, |state, cx| {
            let moved = state.move_selection(delta, extend);
            cx.notify();
            moved
        });
        if moved.is_some() {
            self.refresh_table(cx);
            self.refresh_inspector(cx);
            // 预览开着就跟着翻到新选中的那个
            if self.preview.is_some() {
                self.open_preview(cx);
            }
        }
    }

    /// 上下键要跳几行：图标视图是一整行，列表视图就是一行。
    pub fn vertical_step(&self, cx: &Context<Self>) -> isize {
        match self.browser().read(cx).view() {
            ViewMode::Icon => self.grid_columns.max(1) as isize,
            ViewMode::List => 1,
        }
    }

    /// 左右键：预览开着就换文件，否则在列表里前后移动一格。
    pub fn move_horizontal(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        if self.preview.is_some() {
            self.step_preview(delta, cx);
        } else {
            self.move_focus(delta, extend, cx);
        }
    }

    pub fn select_all_entries(&mut self, cx: &mut Context<Self>) {
        self.browser().update(cx, |state, cx| {
            state.select_all();
            cx.notify();
        });
        self.refresh_table(cx);
    }

    /// Ctrl+Shift+C：复制选中项的完整 key，多选就一行一个。
    pub fn copy_keys(&mut self, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let keys: Vec<String> = self
            .browser()
            .read(cx)
            .selected_entries()
            .iter()
            .map(|e| ops::item_key(&location, &e.name, e.is_dir))
            .collect();
        if keys.is_empty() {
            self.toast_warn("先选点东西", cx);
            return;
        }
        let count = keys.len();
        self.copy_text(
            keys.join(
                "
",
            ),
            format!("已复制 {count} 个 key"),
            cx,
        );
    }

    /// Ctrl+F：把光标送进搜索框。
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search()
            .update(cx, |state, cx| state.focus(window, cx));
    }

    /// Ctrl+Shift+G：输入一段相对当前桶的路径直接跳过去。
    pub fn go_to_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        // 预填当前位置，改一段比从头敲省事
        let current = location.prefix.clone();
        self.open_input(InputPurpose::GoTo, "前往路径", &current, window, cx);
    }
}
