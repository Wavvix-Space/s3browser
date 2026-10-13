//! 主窗口：窗口本身的状态、布局骨架和动作分发。
//!
//! 具体每件事怎么做拆在旁边几个文件里，这里只把它们挂到键位和菜单上：
//!
//! | 文件 | 管什么 |
//! |---|---|
//! | `tab_bar` | 标签页：开、切、关，标签条怎么画 |
//! | `navigate` | 位置切换和目录加载 |
//! | `transfer_ops` | 上传下载复制粘贴，都归结成往引擎里塞一批任务 |
//! | `file_ops` | 新建、重命名、删除，以及它们的对话框回调 |
//! | `share` | 分享链接和上传链接 |
//! | `keyboard` | 方向键、全选那些 |
//! | `drag` | 拖放的元素装饰 |
//!
//! 留在这儿的是：窗口结构体、引擎事件、跟用户说话的那几件小事（右下角提示、
//! 往剪贴板里放东西），以及 `Render`。

use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{table::*, *};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use transfer::engine::EngineEvent;

use crate::app_state::AppState;
use crate::views::account_dialog::AccountDialog;
use crate::views::browser_tab::{BrowserTab, Tab, TabKind};
use crate::views::dialogs::{ConfirmDialog, InputDialog};
use crate::views::icon_view::columns_for;
use crate::views::inspector::INSPECTOR_WIDTH;
use crate::views::menus::{directory_menu, entry_menu};
use crate::views::preview::Preview;
use crate::views::transfer_ops::Clipboard;
use crate::views::transfers::{TransferQueue, TransferTable};
use crate::views::updates::UpdatePhase;
use crate::{
    BROWSER_CONTEXT, CloseAllTabs, CloseOtherTabs, ClosePreview, CloseTab, CloseTabsLeft,
    CloseTabsRight, CloseThisTab, CopyItems, CopyKey, CutItems, DeleteSelected, Download,
    EditAccount, EditFile, FavoriteBucket, FavoriteEntry, FocusSearch, GoBack, GoForward, GoTo,
    GoUp, HideBucket, MoveDown, MoveLeft, MoveRight, MoveUp, NewFolder, OnlyShowBucket,
    OpenEntry, OpenFavorite, OpenSettings, PasteItems, Refresh, RefreshBuckets, RemoveAccount,
    RemoveFavorite, Rename, RenameFavorite, SelectAll, ShareLink, ShowTransfers,
    ToggleFavorite, ToggleInspector, TogglePreview, ToggleShowHidden, UnhideAll, UploadFiles,
    UploadFolder, ViewIcon, ViewList,
};
use s3browser::browser_state::{ViewMode, format_size};
use s3browser::tabs::CloseScope;
use transfer::job::JobKind;

/// 每页条目数。S3 协议上限就是 1000。
pub(crate) const PAGE_SIZE: u32 = 1000;

/// 状态栏高度，内容区按它留出底部空间。提示浮层也按它抬高，免得压在上面。
pub const STATUS_BAR_HEIGHT: f32 = 26.0;

/// 当前打开的浮层。
#[derive(Clone)]
pub enum Dialog {
    Account(Entity<AccountDialog>),
    Input(Entity<InputDialog>),
    Confirm(Entity<ConfirmDialog>),
}

impl Dialog {
    fn element(&self) -> AnyElement {
        match self {
            Dialog::Account(entity) => entity.clone().into_any_element(),
            Dialog::Input(entity) => entity.clone().into_any_element(),
            Dialog::Confirm(entity) => entity.clone().into_any_element(),
        }
    }
}

pub struct MainWindow {
    pub app: Entity<AppState>,
    pub queue: Entity<TransferQueue>,
    /// 传输页的表格。任务和历史共用，行数跟着 `queue` 走。
    pub transfer_table: Entity<TableState<TransferTable>>,
    /// 标签条上的所有格子：打开的桶，外加开着的传输和设置。
    /// 永远至少留一格浏览页（可以是空白页）。
    pub tabs: Vec<Tab>,
    /// 当前是哪一格。
    pub active: usize,
    /// 文件操作作用在哪个桶上。站到传输或设置格时它不变，切回来还是那个桶。
    pub(crate) browsing_id: u64,
    pub(crate) next_tab_id: u64,
    pub dialog: Option<Dialog>,
    /// 剪贴板：复制或剪切的条目，等着粘贴。
    pub clipboard: Option<Clipboard>,
    /// 推送右下角提示要用它：提示归 Root 管，而 Context 里没有 Window。
    window: AnyWindowHandle,
    /// 右键的是侧边栏哪个账户。菜单项不带参数，只能先记下来。
    pub context_profile: Option<String>,
    /// 右键的是侧边栏哪个桶：它的位置，和它在 `BucketView` 里的 key。
    ///
    /// 记值不记索引——菜单是下一帧才建的，中间桶列表要是在后台刷新过，
    /// 索引就指到别人头上了。key 当场从 `BucketEntry::key()` 拿来记下，
    /// 不从 `Location` 反算：算法只留一份，写两份早晚对不上。
    pub context_bucket: Option<(s3browser::location::Location, String)>,
    /// 右键的是第几个收藏项。
    pub context_favorite: Option<usize>,
    /// 侧边栏的拖拽正悬在哪条缝上。域 + 缝号，`None` 表示没悬在任何地方。
    pub(crate) drop_slot: Option<(crate::views::sidebar::dnd::DropDomain, usize)>,
    /// 右键的是标签条上哪一格。同上，菜单项不带参数。记 id 不记索引——
    /// 菜单是下一帧才建的，中间要是关掉了几格，索引就指到别人头上了。
    pub(crate) context_tab: Option<u64>,
    /// 快速预览浮层，同时只留一份内容。
    pub preview: Option<Preview>,
    /// 预览加载的序号，翻到下一个文件后旧响应据此丢弃。
    pub preview_token: u64,
    /// 属性面板开着没有。
    pub inspector: bool,
    /// 一格浏览页都没开时顶着用的空白状态。不在 `tabs` 里，界面上看不见。
    ///
    /// 有它，"当前是哪个桶"就永远有答案，不用把 `browser()` 改成返回
    /// `Option` 再在六十多处判空；也才敢让用户把最后一个浏览页关掉——
    /// 原来关了会立刻补一张一模一样的，看着像没关掉。
    pub(crate) blank: BrowserTab,
    /// 图标视图每行几列，上下键要按它整行跳。渲染时算好。
    pub(crate) grid_columns: usize,
    /// 上次检查更新的结果。设置页「关于」照着它显示。
    pub update_status: s3browser::update::UpdateStatus,
    /// 「立刻更新」进行到哪了。整套流程在 `views/updates.rs`。
    pub update_phase: UpdatePhase,
    /// 设置页的滚动。更新提示里点「查看」要跳到「关于」，得能动它。
    pub(crate) settings_scroll: ScrollHandle,
    focus: FocusHandle,
}

impl MainWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app = cx.new(|_| AppState::new());
        let ui = app.read(cx).config.ui.clone();
        crate::apply_theme(ui.theme, Some(window), cx);
        // 选了"跟随系统"的话，系统白天黑夜一变这儿就跟着变。
        // 选死了浅色深色的不跟——那是用户明说了要哪个。
        window
            .observe_window_appearance({
                let app = app.clone();
                move |window, cx| {
                    let choice = app.read(cx).config.ui.theme;
                    if choice == s3browser::config::Theme::System {
                        crate::apply_theme(choice, Some(window), cx);
                    }
                }
            })
            .detach();
        // 开一格空白浏览页。第一次点桶就开在它上面，不会平白多出一格。
        let first = Tab {
            id: 0,
            kind: TabKind::Browser(BrowserTab::new(0, ui.view, ui.folders_first, window, cx)),
        };
        // 顶替用的那份，id 给个永远不会发出去的值
        let blank = BrowserTab::new(u64::MAX, ui.view, ui.folders_first, window, cx);

        let queue = cx.new(|_| TransferQueue::default());
        let transfer_delegate = TransferTable::new(queue.clone(), cx.entity().downgrade());
        let transfer_table = cx.new(|cx| TableState::new(transfer_delegate, window, cx));

        // 引擎的事件流是运行时无关的通道，直接在 GPUI 的执行器上读
        if let Some(events) = app.update(cx, |app, _| app.engine_events.take()) {
            cx.spawn(async move |this, cx| {
                while let Ok(event) = events.recv().await {
                    if this
                        .update(cx, |this, cx| this.on_engine_event(event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        MainWindow {
            app,
            queue,
            transfer_table,
            tabs: vec![first],
            active: 0,
            browsing_id: 0,
            next_tab_id: 1,
            blank,
            dialog: None,
            clipboard: None,
            window: window.window_handle(),
            context_profile: None,
            context_bucket: None,
            context_favorite: None,
            drop_slot: None,
            context_tab: None,
            preview: None,
            preview_token: 0,
            inspector: false,
            grid_columns: 1,
            update_status: s3browser::update::UpdateStatus::default(),
            update_phase: UpdatePhase::default(),
            settings_scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
        }
    }

    fn on_engine_event(&mut self, event: EngineEvent, cx: &mut Context<Self>) {
        match event {
            EngineEvent::Snapshot {
                jobs,
                total_rate_bps,
                ..
            } => {
                self.queue.update(cx, |queue, cx| {
                    queue.jobs = jobs;
                    queue.total_rate_bps = total_rate_bps;
                    cx.notify();
                });
                self.refresh_transfer_table(cx);
            }
            EngineEvent::JobState { id, state, error } => {
                self.queue.update(cx, |queue, cx| {
                    if let Some(job) = queue.jobs.iter_mut().find(|j| j.id == id) {
                        job.state = state;
                        job.error = error;
                        cx.notify();
                    }
                });
                self.refresh_transfer_table(cx);
            }
            // 之前这条路只在引擎里写日志，界面上无声无息，
            // 用户以为传上了其实什么都没发生。
            EngineEvent::EnqueueFailed { error } => {
                self.toast_err(format!("任务没入队：{error}"), cx);
            }
            EngineEvent::History { entries } => {
                self.queue.update(cx, |queue, cx| {
                    queue.history = entries;
                    cx.notify();
                });
                self.refresh_transfer_table(cx);
            }
            EngineEvent::HistoryAppended { entry } => {
                self.queue.update(cx, |queue, cx| {
                    queue.history.insert(0, entry);
                    cx.notify();
                });
                self.refresh_transfer_table(cx);
            }
            EngineEvent::BatchDone {
                kind,
                label,
                done,
                failed,
                cancelled,
                ..
            } => {
                // 传完一批就把当前目录刷一下，让新对象露出来。
                // 下载只往本地写，远端没变，不用重列。
                if kind != JobKind::Download {
                    self.refresh(cx);
                }
                self.report_batch(&label, done, failed, cancelled, cx);
            }
        }
        cx.notify();
    }

    /// 引擎推来的数据变了，传输表格跟着重画一张。
    fn refresh_transfer_table(&mut self, cx: &mut Context<Self>) {
        self.transfer_table.update(cx, |_, cx| cx.notify());
    }

    /// 一批任务全部落定之后在右下角报一句。
    ///
    /// 全是取消的不报——那是用户自己按的，他知道。
    fn report_batch(
        &mut self,
        label: &str,
        done: u32,
        failed: u32,
        cancelled: u32,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(label, done, failed, cancelled, "一批传输结束");
        if done == 0 && failed == 0 {
            return;
        }
        if failed == 0 {
            self.toast_ok(format!("{label} 完成"), cx);
        } else if done == 0 {
            self.toast_err(format!("{label} 失败"), cx);
        } else {
            self.toast_warn(format!("{label}：{done} 项成功，{failed} 项失败"), cx);
        }
    }

    pub(crate) fn copy_link(&mut self, url: String, cx: &mut Context<Self>) {
        tracing::info!("分享链接已复制");
        self.copy_text(url, "链接已复制到剪贴板", cx);
    }

    /// 往剪贴板写一段文字，状态栏给个回执。
    pub fn copy_text(
        &mut self,
        text: String,
        toast: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.toast_ok(toast, cx);
    }

    /// 右下角冒一条提示，几秒后自己消失。
    ///
    /// 提示是挂在 Root 上的，推送要 `&mut Window`，而这里到处都只有 `Context`。
    /// 所以存了窗口句柄，用 defer 在本帧之后推——顺便也避开了在渲染中途
    /// 去改别的 entity。
    pub fn toast(&mut self, note: Notification, cx: &mut Context<Self>) {
        let window = self.window;
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| window.push_notification(note, cx))
                .ok();
        });
    }

    /// 办成了。
    pub fn toast_ok(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast(Notification::success(message), cx);
    }

    /// 进行中之类的中性消息。
    pub fn toast_info(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast(Notification::info(message), cx);
    }

    /// 没按用户预期做成，但不是错误——比如没选东西就按了分享。
    pub fn toast_warn(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast(Notification::warning(message), cx);
    }

    /// 真出错了。
    pub fn toast_err(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast(Notification::error(message), cx);
    }

    /// 内容区。右键菜单只挂在这一处。
    ///
    /// 行和图标格只负责右键时选中自己并挡住冒泡，没挡住就说明点在空白处，
    /// 这里把选择清掉。菜单按清完之后的选择状态决定给条目菜单还是目录菜单。
    /// 两个视图共用这一套，表格 delegate 的 context_menu 不用——那个回调跑在
    /// TableState 的 update 里，在里面碰表格就是重入更新，GPUI 会 panic。
    fn wrap_content(&mut self, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let inner = div()
            .id("content")
            .size_full()
            .overflow_hidden()
            // 从资源管理器拖文件进来就传到当前目录
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.15)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.drop_into_folder(None, paths.paths().to_vec(), cx);
            }))
            .child(content);

        // 传输和设置格没有条目可操作，不给菜单
        if !self.on_browser_tab() {
            return div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(inner)
                .into_any_element();
        }

        let main = cx.entity().downgrade();
        div()
            .id("content-area")
            .flex_1()
            .min_h_0()
            .w_full()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.browser().update(cx, |state, cx| {
                        state.clear_selection();
                        cx.notify();
                    });
                    this.refresh_table(cx);
                }),
            )
            .child(inner)
            .context_menu(move |menu, _, cx| {
                // 选中项和剪贴板都在开菜单这一刻现读。
                // 渲染时算好塞进闭包的话，"复制"完了菜单里的"粘贴"还是灰的——
                // 中间那次 notify 不一定赶在右键之前落到这个闭包上。
                let Some(main) = main.upgrade() else {
                    return menu;
                };
                let this = main.read(cx);
                let state = this.browser().read(cx);
                let selected = state.selected_count();
                let can_paste = this.clipboard.is_some();
                // 收藏那一项正反两用，文案跟着现状走
                if selected == 0 {
                    let favorited = state
                        .location()
                        .is_some_and(|here| this.is_favorited(here, cx));
                    directory_menu(menu, can_paste, favorited)
                } else {
                    let folder = state
                        .selected_entries()
                        .into_iter()
                        .find(|e| e.is_dir)
                        .map(|e| e.name.to_string());
                    let favorited = state
                        .location()
                        .zip(folder)
                        .is_some_and(|(here, folder)| this.is_favorited(&here.child(&folder), cx));
                    entry_menu(menu, selected == 1, can_paste, favorited)
                }
            })
            .into_any_element()
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let left = match self.tab().kind {
            TabKind::Browser(_) => self.browse_status(cx),
            TabKind::Transfers => self.transfer_status(cx),
            TabKind::Settings(_) => "改完立刻生效，不用按保存".to_owned(),
        };

        h_flex()
            .w_full()
            .h(px(STATUS_BAR_HEIGHT))
            .flex_shrink_0()
            .px_3()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().title_bar)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(left)
    }

    fn browse_status(&self, cx: &Context<Self>) -> String {
        let state = self.browser().read(cx);
        let count = state.visible_count();
        let selected = state.selected_count();
        if state.location().is_none() {
            return "未连接".to_owned();
        }
        if state.is_loading() {
            return format!("正在加载… 已有 {count} 项");
        }
        let mut text = format!("{count} 项，共 {}", format_size(state.total_size()));
        if selected > 0 {
            text.push_str(&format!(" · 已选 {selected} 项"));
        }
        text
    }

    fn transfer_status(&self, cx: &Context<Self>) -> String {
        let queue = self.queue.read(cx);
        if queue.jobs.is_empty() {
            return "没有传输任务".to_owned();
        }
        let mut text = format!("{} 个任务", queue.jobs.len());
        let active = queue.active_count();
        if active > 0 {
            text.push_str(&format!(" · {active} 个在传"));
        }
        if queue.total_rate_bps > 0 {
            text.push_str(&format!(" · {}/s", format_size(queue.total_rate_bps)));
        }
        let failed = queue.failed_count();
        if failed > 0 {
            text.push_str(&format!(" · {failed} 个失败"));
        }
        text
    }
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 根容器要按 viewport_size 给显式尺寸，不能用 size_full()：
        // size_full 是相对父容器的百分比，而 Root 那层的尺寸算出来不是窗口大小，
        // 结果高度分配偏掉，排在最后的状态栏和侧边栏底部会被挤出容器，一个像素都画不出来。
        // viewport_size 本身已经是逻辑像素（乘 scale_factor 才是物理像素），直接用就行。
        let viewport = window.viewport_size();
        let sidebar_width = px(self.app.read(cx).config.ui.sidebar_width);
        let show_inspector = self.inspector && self.on_browser_tab();
        let inspector_width = if show_inspector {
            px(INSPECTOR_WIDTH)
        } else {
            px(0.)
        };
        // 图标视图要按确定的宽度算列数，所以这里自己减，不靠布局测量
        let content_width = (viewport.width - sidebar_width - inspector_width).max(px(120.));
        self.grid_columns = columns_for(content_width);

        let view = self.browser().read(cx).view();
        let content: AnyElement = match self.tab().kind {
            TabKind::Transfers => self.render_transfers(cx).into_any_element(),
            TabKind::Settings(_) => self.render_settings(cx),
            TabKind::Browser(_) => match view {
                ViewMode::List => DataTable::new(&self.table())
                    .stripe(true)
                    .into_any_element(),
                ViewMode::Icon => self.render_icon_view(content_width, cx),
            },
        };
        let inspector = show_inspector.then(|| self.render_inspector(cx).into_any_element());
        // 地址栏是浏览格的东西，传输和设置没有路径可言
        let path_bar = self
            .on_browser_tab()
            .then(|| self.render_path_bar(cx).into_any_element());
        let preview = self
            .preview
            .as_ref()
            .map(|preview| self.render_preview(preview, cx));

        v_flex()
            .key_context(BROWSER_CONTEXT)
            .track_focus(&self.focus)
            // 对话框开着的时候，列表的快捷键一律不接。之前 Enter 会漏到文件
            // 列表上触发"打开"，把身后的目录切走，确认框再按那个新位置去删。
            // 站在传输或设置格上也不接——那儿没有文件可操作。
            // 标签本身的操作在哪一格上都该管用，只受对话框拦截
            .when(self.dialog.is_none(), |this| {
                this.on_action(
                    cx.listener(|this, _: &ShowTransfers, _, cx| this.show_transfers(cx)),
                )
                .on_action(
                    cx.listener(|this, _: &OpenSettings, window, cx| {
                        this.show_settings(window, cx)
                    }),
                )
                .on_action(
                    cx.listener(|this, _: &CloseTab, window, cx| this.close_active_tab(window, cx)),
                )
                // 标签条右键菜单。这几件事作用在右键点中的那一格上
                .on_action(cx.listener(|this, _: &CloseThisTab, window, cx| {
                    this.close_context_tab(window, cx)
                }))
                .on_action(cx.listener(|this, _: &CloseOtherTabs, window, cx| {
                    this.close_tabs(CloseScope::Others, window, cx)
                }))
                .on_action(cx.listener(|this, _: &CloseTabsLeft, window, cx| {
                    this.close_tabs(CloseScope::Left, window, cx)
                }))
                .on_action(cx.listener(|this, _: &CloseTabsRight, window, cx| {
                    this.close_tabs(CloseScope::Right, window, cx)
                }))
                .on_action(cx.listener(|this, _: &CloseAllTabs, window, cx| {
                    this.close_tabs(CloseScope::All, window, cx)
                }))
            })
            .when(self.dialog.is_none() && self.on_browser_tab(), |this| {
                this.on_action(
                    cx.listener(|this, _: &NewFolder, window, cx| this.new_folder(window, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &Rename, window, cx| this.rename_selected(window, cx)),
                )
                .on_action(cx.listener(|this, _: &DeleteSelected, window, cx| {
                    this.delete_selected(window, cx)
                }))
                .on_action(cx.listener(|this, _: &CopyItems, _, cx| this.copy_selected(false, cx)))
                .on_action(cx.listener(|this, _: &CutItems, _, cx| this.copy_selected(true, cx)))
                .on_action(cx.listener(|this, _: &PasteItems, _, cx| this.paste(cx)))
                .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
                .on_action(cx.listener(|this, _: &GoUp, window, cx| this.go_up(window, cx)))
                .on_action(cx.listener(|this, _: &GoBack, window, cx| this.go_back(window, cx)))
                .on_action(
                    cx.listener(|this, _: &GoForward, window, cx| this.go_forward(window, cx)),
                )
                .on_action(cx.listener(|this, _: &ShareLink, _, cx| this.share_selected(cx)))
                .on_action(
                    cx.listener(|this, _: &EditAccount, window, cx| this.edit_account(window, cx)),
                )
                .on_action(cx.listener(|this, _: &RefreshBuckets, _, cx| this.refresh_buckets(cx)))
                .on_action(cx.listener(|this, _: &RemoveAccount, window, cx| {
                    this.remove_account(window, cx)
                }))
                .on_action(cx.listener(|this, _: &ToggleFavorite, _, cx| {
                    this.toggle_favorite_here(cx)
                }))
                .on_action(cx.listener(|this, _: &FavoriteEntry, _, cx| {
                    this.favorite_selected_entry(cx)
                }))
                .on_action(cx.listener(|this, _: &FavoriteBucket, _, cx| {
                    this.favorite_context_bucket(cx)
                }))
                .on_action(cx.listener(|this, _: &OpenFavorite, window, cx| {
                    this.open_context_favorite(window, cx)
                }))
                .on_action(cx.listener(|this, _: &RenameFavorite, window, cx| {
                    this.rename_favorite(window, cx)
                }))
                .on_action(cx.listener(|this, _: &RemoveFavorite, _, cx| {
                    this.remove_context_favorite(cx)
                }))
                .on_action(cx.listener(|this, _: &HideBucket, _, cx| {
                    this.toggle_hide_context_bucket(cx)
                }))
                .on_action(cx.listener(|this, _: &OnlyShowBucket, _, cx| {
                    this.only_show_context_bucket(cx)
                }))
                .on_action(cx.listener(|this, _: &ToggleShowHidden, _, cx| {
                    this.toggle_show_hidden(cx)
                }))
                .on_action(cx.listener(|this, _: &UnhideAll, _, cx| this.unhide_all(cx)))
                .on_action(
                    cx.listener(|this, _: &OpenEntry, window, cx| this.open_entry(window, cx)),
                )
                .on_action(cx.listener(|this, _: &TogglePreview, _, cx| this.toggle_preview(cx)))
                .on_action(cx.listener(|this, _: &ClosePreview, _, cx| this.close_preview(cx)))
                .on_action(
                    cx.listener(|this, _: &ToggleInspector, _, cx| this.toggle_inspector(cx)),
                )
                .on_action(
                    cx.listener(|this, _: &ViewList, _, cx| this.set_view(ViewMode::List, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &ViewIcon, _, cx| this.set_view(ViewMode::Icon, cx)),
                )
                .on_action(cx.listener(|this, _: &UploadFiles, _, cx| this.upload_files(cx)))
                .on_action(cx.listener(|this, _: &UploadFolder, _, cx| this.upload_folder(cx)))
                .on_action(cx.listener(|this, _: &Download, _, cx| this.download_selected(cx)))
                .on_action(cx.listener(|this, _: &CopyKey, _, cx| this.copy_keys(cx)))
                .on_action(cx.listener(|this, _: &EditFile, _, cx| this.edit_selected(cx)))
                .on_action(cx.listener(|this, _: &GoTo, window, cx| this.go_to_path(window, cx)))
                .on_action(
                    cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
                )
                .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all_entries(cx)))
                .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                    let step = this.vertical_step(cx);
                    this.move_focus(-step, false, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                    let step = this.vertical_step(cx);
                    this.move_focus(step, false, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &MoveLeft, _, cx| this.move_horizontal(-1, false, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &MoveRight, _, cx| this.move_horizontal(1, false, cx)),
                )
            })
            .w(viewport.width)
            .h(viewport.height)
            .overflow_hidden()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(window, cx))
            .child(
                h_flex()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_sidebar(sidebar_width, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_tabs(cx))
                            // 地址栏是浏览页的东西，传输页没有路径可言
                            .children(path_bar)
                            .child(self.wrap_content(content, cx)),
                    )
                    .children(inspector),
            )
            .child(self.render_status_bar(cx))
            .children(self.dialog.as_ref().map(|d| d.element()))
            .children(preview)
            // 右下角的提示层。Root 自己不画它，得由应用挂在自己的树上，
            // 放最后一个才压得住对话框和预览浮层。
            .children(Root::render_notification_layer(window, cx))
    }
}
