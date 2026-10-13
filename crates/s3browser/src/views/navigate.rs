//! 位置切换和目录加载。
//!
//! 所有导航最后都汇到 `load_directory`：它管代际号，导航离开之后旧请求
//! 回来直接丢掉。每个标签页各有一份代际号，按标签 id 找回自己那一份——
//! 请求跑回来的时候索引可能已经挪了。

use gpui_kit::*;
use s3api::S3Client;
use std::sync::Arc;

use s3browser::browser_state::Location;

use crate::views::inspector::InspectorExtra;
use crate::views::main_window::{MainWindow, PAGE_SIZE};

impl MainWindow {
    pub fn open_location(
        &mut self,
        location: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.browser().update(cx, |state, cx| {
            state.navigate_to(location);
            cx.notify();
        });
        self.clear_search(window, cx);
        self.load_directory(cx);
    }

    pub fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.browser().update(cx, |state, cx| {
            let moved = state.go_back();
            cx.notify();
            moved
        }) {
            self.clear_search(window, cx);
            self.load_directory(cx);
        }
    }

    pub fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.browser().update(cx, |state, cx| {
            let moved = state.go_forward();
            cx.notify();
            moved
        }) {
            self.clear_search(window, cx);
            self.load_directory(cx);
        }
    }

    pub fn go_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.browser().update(cx, |state, cx| {
            let moved = state.go_up();
            cx.notify();
            moved
        }) {
            self.clear_search(window, cx);
            self.load_directory(cx);
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        self.browser().update(cx, |state, cx| {
            state.reload(location);
            cx.notify();
        });
        self.load_directory(cx);
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search().read(cx).value().is_empty() {
            self.search().update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
    }

    /// 行被点击。双击文件夹进去，单击只改选择。
    pub fn on_row_click(
        &mut self,
        row: usize,
        ctrl: bool,
        shift: bool,
        double: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = self.browser().read(cx).row(row).cloned();
        self.browser().update(cx, |state, cx| {
            state.click_row(row, ctrl, shift);
            cx.notify();
        });
        self.refresh_table(cx);
        self.refresh_inspector(cx);

        if double && let Some(entry) = entry {
            if entry.is_dir {
                let Some(location) = self.browser().read(cx).location().cloned() else {
                    return;
                };
                self.open_location(location.child(&entry.name), window, cx);
            } else {
                // 双击文件走快速预览，和 Enter 一个行为
                self.open_preview(cx);
            }
        }
    }

    pub(crate) fn client_for(
        &mut self,
        profile_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<S3Client>> {
        self.app.update(cx, |app, _| app.client(profile_id))
    }

    /// 拉当前目录的第一页。
    fn load_directory(&mut self, cx: &mut Context<Self>) {
        // 所有导航路径最后都会走到这里，浮层和属性面板里的东西这时候都过期了
        self.preview = None;
        self.browsing_mut().extra = InspectorExtra::default();

        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let Some(client) = self.client_for(&location.profile, cx) else {
            self.browser().update(cx, |state, cx| {
                state.set_error("这个账户还不能用，请检查配置");
                cx.notify();
            });
            self.refresh_table(cx);
            return;
        };

        let tab_id = self.browsing().id;
        let tab = self.browsing_mut();
        tab.generation += 1;
        tab.paging = false;
        let generation = tab.generation;
        let browser = tab.browser.clone();

        cx.spawn(async move |this, cx| {
            let result = client
                .list_objects_v2(
                    &location.bucket,
                    &location.prefix,
                    Some("/"),
                    None,
                    PAGE_SIZE,
                )
                .await;

            this.update(cx, |this, cx| {
                // 这页已经导航去别处、或者整页被关掉了，这批数据不要了
                if this
                    .tab_by_id(tab_id)
                    .is_none_or(|t| t.generation != generation)
                {
                    return;
                }
                browser.update(cx, |state, cx| {
                    match result {
                        Ok(page) => {
                            tracing::debug!(
                                bucket = %location.bucket,
                                prefix = %location.prefix,
                                objects = page.objects.len(),
                                dirs = page.prefixes.len(),
                                "目录已加载"
                            );
                            state.set_page(&page.objects, &page.prefixes, page.next_token)
                        }
                        Err(e) => {
                            tracing::warn!(
                                bucket = %location.bucket,
                                prefix = %location.prefix,
                                "列举失败：{e}"
                            );
                            state.set_error(e.to_string())
                        }
                    }
                    cx.notify();
                });
                this.notify_table(tab_id, cx);
            })
            .ok();
        })
        .detach();
    }

    /// 表格滚到底时继续拉下一页。
    pub fn load_next_page(&mut self, cx: &mut Context<Self>) {
        // 图标视图按滚动位置触发翻页，一帧里会问好几次，没这道闸门就会重复请求
        if self.browsing().paging {
            return;
        }
        let (Some(location), Some(token)) = (
            self.browser().read(cx).location().cloned(),
            self.browser().read(cx).next_token().map(str::to_owned),
        ) else {
            return;
        };
        let Some(client) = self.client_for(&location.profile, cx) else {
            return;
        };

        let tab_id = self.browsing().id;
        let tab = self.browsing_mut();
        let generation = tab.generation;
        let browser = tab.browser.clone();
        tab.paging = true;

        cx.spawn(async move |this, cx| {
            let result = client
                .list_objects_v2(
                    &location.bucket,
                    &location.prefix,
                    Some("/"),
                    Some(&token),
                    PAGE_SIZE,
                )
                .await;

            this.update(cx, |this, cx| {
                let Some(tab) = this.tab_by_id(tab_id) else {
                    return;
                };
                tab.paging = false;
                if tab.generation != generation {
                    return;
                }
                browser.update(cx, |state, cx| {
                    match result {
                        Ok(page) => {
                            state.append_page(&page.objects, &page.prefixes, page.next_token)
                        }
                        Err(e) => state.set_error(e.to_string()),
                    }
                    cx.notify();
                });
                this.notify_table(tab_id, cx);
            })
            .ok();
        })
        .detach();
    }

    pub fn refresh_table(&mut self, cx: &mut Context<Self>) {
        let id = self.browsing().id;
        self.notify_table(id, cx);
    }

    /// 戳指定那一页的表格。后台页加载完也得重画，不然切回去还是旧的。
    fn notify_table(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(table) = self.tab_by_id(tab_id).map(|t| t.table.clone()) else {
            return;
        };
        table.update(cx, |_, cx| cx.notify());
    }
}
