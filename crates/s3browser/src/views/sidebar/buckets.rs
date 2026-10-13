//! 账户底下的桶：从服务端拉列表，以及本地的排序和隐藏。
//!
//! 桶列表只喂侧边栏这一棵树，跟着它放在一起更好找。
//! 顺序和隐藏名单是纯本地的事（`config::BucketView`），不碰服务端。

use gpui_kit::*;

use super::dnd::DropDomain;
use crate::views::main_window::MainWindow;
use s3browser::buckets::BucketEntry;
use s3browser::config::BucketView;

impl MainWindow {
    /// 展开账户时拉它的桶列表。
    pub fn load_buckets(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let Some(profile) = self.app.read(cx).config.profile(profile_id).cloned() else {
            return;
        };

        // 配置里写死了桶就直接用，不去问服务端
        if !profile.buckets.is_empty() {
            let entries: Vec<BucketEntry> = profile
                .buckets
                .iter()
                .map(|b| BucketEntry::bucket(b.clone()))
                .collect();
            self.set_buckets(profile_id, Ok(entries), cx);
            return;
        }

        self.list_buckets(profile_id, cx);
    }

    /// ListBuckets。
    fn list_buckets(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let Some(client) = self.client_for(profile_id, cx) else {
            self.set_buckets(profile_id, Err("这个账户还不能用".into()), cx);
            return;
        };
        self.mark_loading(profile_id, cx);

        let profile_id = profile_id.to_owned();
        cx.spawn(async move |this, cx| {
            let result = client.list_buckets().await;
            this.update(cx, |this, cx| {
                let entries = result
                    .map(|buckets| {
                        buckets
                            .into_iter()
                            .map(|b| BucketEntry::bucket(b.name))
                            .collect()
                    })
                    .map_err(|e| e.to_string());
                this.set_buckets(&profile_id, entries, cx);
            })
            .ok();
        })
        .detach();
    }

    fn mark_loading(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| {
            let entry = app.buckets.entry(profile_id.to_owned()).or_default();
            entry.loading = true;
            entry.error = None;
            cx.notify();
        });
    }

    fn set_buckets(
        &mut self,
        profile_id: &str,
        result: Result<Vec<BucketEntry>, String>,
        cx: &mut Context<Self>,
    ) {
        self.app.update(cx, |app, cx| {
            let entry = app.buckets.entry(profile_id.to_owned()).or_default();
            entry.loading = false;
            entry.loaded = true;
            match result {
                Ok(buckets) => {
                    entry.buckets = buckets;
                    entry.error = None;
                }
                Err(message) => entry.error = Some(message),
            }
            cx.notify();
        });
    }

    /// 展开或收起一个账户。第一次展开时去拉桶列表。
    pub fn toggle_profile(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let (now_expanded, needs_load) = self.app.update(cx, |app, cx| {
            let expanded = app.expanded.entry(profile_id.to_owned()).or_insert(false);
            *expanded = !*expanded;
            let now = *expanded;
            cx.notify();
            (now, now && !app.bucket_list(profile_id).loaded)
        });
        if now_expanded && needs_load {
            self.load_buckets(profile_id, cx);
        }
        cx.notify();
    }

    /// 藏起右键点中的那个桶；已经藏着的就放出来。
    ///
    /// 藏着的桶只有开着「显示隐藏的桶」时才看得见、才右键得到，这时同一个
    /// 菜单项反过来用，就不必再单独做一个「取消隐藏」的动作。
    pub fn toggle_hide_context_bucket(&mut self, cx: &mut Context<Self>) {
        let Some((at, key)) = self.context_bucket.clone() else {
            return;
        };
        self.update_view(&at.profile, cx, |view, _, _| view.toggle_hidden(&key));
    }

    /// 只显示右键点中的那个，其余全藏起来。
    ///
    /// 七牛那种「拉回来二十个只有一个能用」的账户，右键十九次太蠢。
    pub fn only_show_context_bucket(&mut self, cx: &mut Context<Self>) {
        let Some((at, key)) = self.context_bucket.clone() else {
            return;
        };
        self.update_view(&at.profile, cx, |view, entries, _| {
            view.only_show(&key, entries)
        });
    }

    /// 临时看一眼藏起来的桶。不落盘：重启回到干净视图。
    pub fn toggle_show_hidden(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.context_profile.clone() else {
            return;
        };
        self.app.update(cx, |app, cx| {
            if !app.show_hidden.remove(&id) {
                app.show_hidden.insert(id);
            }
            cx.notify();
        });
        cx.notify();
    }

    /// 把这个账户藏起来的桶全放出来，连名单里那些已经对不上号的旧 key 一起清掉。
    pub fn unhide_all(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.context_profile.clone() else {
            return;
        };
        self.update_view(&id, cx, |view, _, _| view.hidden.clear());
    }

    /// 桶拖拽落下：挪到当前悬着的那条缝上。缝号按眼前看到的行数，
    /// 换算和物化顺序的活都在 `BucketView::move_to` 里。
    pub fn drop_bucket(&mut self, profile_id: &str, key: &str, cx: &mut Context<Self>) {
        let Some(slot) = self.take_drop_slot(&DropDomain::Buckets(profile_id.to_owned())) else {
            return;
        };
        self.update_view(profile_id, cx, |view, entries, show_hidden| {
            view.move_to(key, slot, entries, show_hidden)
        });
    }

    /// 改一个账户的 `BucketView` 并落盘。
    ///
    /// `f` 拿到的是眼下的桶列表，和「显示隐藏的桶」开没开——排序时缝号是按
    /// 眼前看到的行数的，它得知道眼前到底显示了哪些。
    fn update_view(
        &mut self,
        profile_id: &str,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut BucketView, &[BucketEntry], bool),
    ) {
        self.app.update(cx, |app, cx| {
            let entries = app.bucket_list(profile_id).buckets;
            let show_hidden = app.shows_hidden(profile_id);
            if let Some(profile) = app.config.profile_mut(profile_id) {
                f(&mut profile.view, &entries, show_hidden);
                app.save_config();
            }
            cx.notify();
        });
        cx.notify();
    }
}
