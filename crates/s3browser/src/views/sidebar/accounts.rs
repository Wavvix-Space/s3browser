//! 账户的增删改和排序。

use gpui_kit::*;

use super::dnd::DropDomain;
use crate::views::account_dialog::AccountDialog;
use crate::views::dialogs::{ConfirmAction, ConfirmDialog};
use crate::views::main_window::MainWindow;

impl MainWindow {
    /// 账户拖拽落下：把它挪到当前悬着的那条缝上。
    pub fn drop_profile(&mut self, dragged: &str, cx: &mut Context<Self>) {
        let Some(slot) = self.take_drop_slot(&DropDomain::Profiles) else {
            return;
        };
        self.app.update(cx, |app, cx| {
            let Some(from) = app.config.profiles.iter().position(|p| p.id == dragged) else {
                return;
            };
            s3browser::reorder::move_to_slot(&mut app.config.profiles, from, slot);
            app.save_config();
            cx.notify();
        });
        cx.notify();
    }

    /// 编辑右键选中的那个账户。
    pub fn edit_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.context_profile.clone() else {
            return;
        };
        self.open_account_dialog(Some(id), window, cx);
    }

    /// 把这个账户的桶列表丢掉重拉。改过权限之后用得上。
    pub fn refresh_buckets(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.context_profile.clone() else {
            return;
        };
        self.app.update(cx, |app, cx| {
            app.invalidate(&id);
            cx.notify();
        });
        self.load_buckets(&id, cx);
        self.toast_info("正在重新拉取桶列表", cx);
    }

    /// 删账户前先问一句。
    pub fn remove_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.context_profile.clone() else {
            return;
        };
        let Some(name) = self
            .app
            .read(cx)
            .config
            .profile(&id)
            .map(|p| p.name.clone())
        else {
            return;
        };

        let main = cx.entity().downgrade();
        let dialog = cx.new(|cx| {
            ConfirmDialog::new(
                ConfirmAction::RemoveAccount {
                    id,
                    name: name.clone(),
                },
                "删除账户",
                // 说清楚只动本地，免得以为连桶里的东西一起删
                format!("要从这个程序里删掉账户 {name} 吗？只删本地配置，桶里的东西一个不动。"),
                main,
                window,
                cx,
            )
        });
        self.dialog = Some(crate::views::main_window::Dialog::Confirm(dialog));
        cx.notify();
    }

    pub fn open_account_dialog(
        &mut self,
        profile_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = profile_id
            .as_deref()
            .and_then(|id| self.app.read(cx).config.profile(id).cloned());
        let main = cx.entity().downgrade();
        let dialog = cx.new(|cx| AccountDialog::new(existing, main, window, cx));
        self.dialog = Some(crate::views::main_window::Dialog::Account(dialog));
        cx.notify();
    }
}
