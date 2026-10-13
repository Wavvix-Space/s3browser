//! 改目录结构：新建文件夹、重命名、删除。
//!
//! 这三样都要先弹个框，所以输入框和确认框的回调也在这里——
//! 对话框只负责收字，收完往哪儿分派是这一层的事。

use gpui_kit::*;
use transfer::job::{Batch, JobKind};

use s3browser::ops;

use crate::views::dialogs::{ConfirmAction, ConfirmDialog, InputDialog, InputPurpose};
use crate::views::main_window::{Dialog, MainWindow};
use crate::views::transfer_ops::Announce;

impl MainWindow {
    /// 弹出新建文件夹的输入框。
    pub fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.browser().read(cx).location().is_none() {
            return;
        }
        self.open_input(InputPurpose::NewFolder, "新建文件夹", "", window, cx);
    }

    /// 重命名选中的那一项。选了多个就不动。
    pub fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let selected = self.browser().read(cx).selected_entries();
        if selected.len() != 1 {
            return;
        }
        let entry = selected[0];
        let name = entry.name.to_string();
        let is_dir = entry.is_dir;
        let key = if is_dir {
            format!("{}{}/", location.prefix, name)
        } else {
            location.key_of(&name)
        };
        self.open_input(
            InputPurpose::Rename { key, is_dir },
            "重命名",
            &name,
            window,
            cx,
        );
    }

    pub(crate) fn open_input(
        &mut self,
        purpose: InputPurpose,
        title: &'static str,
        initial: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_input_with(purpose, title, initial, None, "名称", window, cx);
    }

    pub(crate) fn open_input_with(
        &mut self,
        purpose: InputPurpose,
        title: &'static str,
        initial: &str,
        hint: Option<SharedString>,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let main = cx.entity().downgrade();
        let initial = initial.to_owned();
        let dialog = cx.new(|cx| {
            InputDialog::new(
                purpose,
                title,
                &initial,
                hint,
                placeholder,
                main,
                window,
                cx,
            )
        });
        self.dialog = Some(Dialog::Input(dialog));
        cx.notify();
    }

    /// 输入框点了确定。
    pub fn on_input_confirmed(
        &mut self,
        purpose: InputPurpose,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 这一项跟"当前在看哪儿"无关，得赶在下面取 location 之前处理掉——
        // 不然站在空白浏览页上改收藏的名字会静默失败
        if let InputPurpose::RenameFavorite { index } = purpose {
            self.apply_favorite_rename(index, name, cx);
            return;
        }
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        match purpose {
            InputPurpose::NewFolder => {
                let batch = Batch::new(JobKind::Upload, String::new());
                let job = ops::new_folder_job(&batch, &location, &name);
                self.submit_batch(
                    batch,
                    vec![job],
                    |_| format!("新建文件夹 {name}"),
                    None,
                    Announce::Quiet,
                    cx,
                );
            }
            InputPurpose::GoTo => {
                let target = location.at(&name);
                if target != location {
                    self.open_location(target, window, cx);
                }
                return;
            }
            InputPurpose::Rename { key, is_dir } => {
                // 文件夹的大小是未知的（列表里显示 --），引擎会自己去数
                let size = if is_dir {
                    0
                } else {
                    self.browser()
                        .read(cx)
                        .selected_entries()
                        .first()
                        .map(|e| e.size)
                        .unwrap_or(0)
                };
                let batch = Batch::new(JobKind::Move, String::new());
                let jobs = ops::rename_job(&batch, &location, &key, &name, size)
                    .into_iter()
                    .collect();
                self.submit_batch(
                    batch,
                    jobs,
                    |_| format!("重命名为 {name}"),
                    None,
                    Announce::Quiet,
                    cx,
                );
            }
            // 函数开头已经接走了，走不到这儿
            InputPurpose::RenameFavorite { .. } => unreachable!("RenameFavorite 在开头就处理掉了"),
        }
        cx.notify();
    }

    /// 删除选中的条目，先问一句。
    pub fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let items = self.browser().read(cx).selected_items();
        if items.is_empty() {
            return;
        }

        let message = if let [only] = items.as_slice() {
            let name = &only.name;
            if only.is_dir {
                format!("要删除文件夹 {name} 和它里面的所有内容吗？删了拿不回来。")
            } else {
                format!("要删除 {name} 吗？删了拿不回来。")
            }
        } else {
            format!("要删除这 {} 项吗？删了拿不回来。", items.len())
        };

        let main = cx.entity().downgrade();
        let action = ConfirmAction::Delete { location, items };
        let dialog = cx.new(|cx| ConfirmDialog::new(action, "删除", message, main, window, cx));
        self.dialog = Some(Dialog::Confirm(dialog));
        cx.notify();
    }

    /// 确认框点了确定。
    pub fn on_confirmed(&mut self, action: ConfirmAction, cx: &mut Context<Self>) {
        match action {
            ConfirmAction::Delete { location, items } => {
                let batch = Batch::new(JobKind::Delete, String::new());
                let jobs = ops::delete_jobs(&batch, &location, &items);
                // 删除是一瞬间的事，不报「开始」，只等收尾那条
                self.submit_batch(
                    batch,
                    jobs,
                    |n| format!("删除 {n} 项"),
                    None,
                    Announce::Quiet,
                    cx,
                );
            }
            ConfirmAction::RemoveAccount { id, name } => {
                // 正看着这个账户的话右边也要清掉，不然还停在删掉的桶上
                let viewing = self
                    .browser()
                    .read(cx)
                    .location()
                    .is_some_and(|l| l.profile == id);
                if viewing {
                    self.browser().update(cx, |state, cx| {
                        state.close();
                        cx.notify();
                    });
                    self.refresh_table(cx);
                }

                self.app.update(cx, |app, cx| {
                    app.invalidate(&id);
                    app.config.remove(&id);
                    app.save_config();
                    cx.notify();
                });
                self.context_profile = None;
                tracing::info!(%id, "账户已删除");
                self.toast_ok(format!("已删除账户 {name}"), cx);
            }
        }
        cx.notify();
    }
}
