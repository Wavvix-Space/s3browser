//! 分享链接。
//!
//! 自己签一个预签名 URL：预签名是对着单个对象签的，所以一次只能分享一个文件。

use gpui_kit::*;

use crate::views::main_window::MainWindow;

/// 分享链接的有效期，七天是 S3 预签名的上限。
const SHARE_EXPIRE_SECS: u64 = 7 * 24 * 3600;

impl MainWindow {
    /// 给选中的对象要一个分享链接，拿到就放进剪贴板。
    pub fn share_selected(&mut self, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let items = self.browser().read(cx).selected_items();
        if items.is_empty() {
            self.toast_warn("先选点东西再分享", cx);
            return;
        }
        if items.iter().any(|item| item.is_dir) {
            self.toast_warn("只能分享文件，文件夹没法签成一条链接", cx);
            return;
        }
        if items.len() > 1 {
            self.toast_warn("一次只能分享一个文件", cx);
            return;
        }
        let Some(client) = self.client_for(&location.profile, cx) else {
            self.toast_err("这个账户还不能用", cx);
            return;
        };
        let key = location.key_of(&items[0].name);
        cx.spawn(async move |this, cx| {
            let result = client
                .presign_get(
                    &location.bucket,
                    &key,
                    std::time::Duration::from_secs(SHARE_EXPIRE_SECS),
                )
                .await;
            this.update(cx, |this, cx| match result {
                Ok(url) => this.copy_link(url, cx),
                Err(e) => this.toast_err(format!("签链接失败：{e}"), cx),
            })
            .ok();
        })
        .detach();
    }
}
