//! 标签条上的一格。
//!
//! 大部分格子是一个桶（`BrowserTab`），各自记着自己的位置、历史、选中和搜索。
//! 传输和设置也各占一格，但它们没有这些状态，所以只是个标记——
//! 全都一视同仁地能开能关，谁也不常驻。

// InputState 和 InputEvent 在 gpui_kit 根上也有一份同名的，
// glob 引进来会撞，这里写全路径。
use gpui_kit::component::IconName;
use gpui_kit::component::VirtualListScrollHandle;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::table::TableState;
use gpui_kit::*;

use s3browser::browser_state::{BrowserState, Location, ViewMode};
use s3browser::tabs::Slot;

use crate::views::inspector::InspectorExtra;
use crate::views::list_view::FileTable;
use crate::views::main_window::MainWindow;
use crate::views::settings::SettingsForm;

/// 标签条上的一格。
pub struct Tab {
    /// 稳定的标识。异步请求回来时按它找格——索引会因为关格而挪位。
    pub id: u64,
    pub kind: TabKind,
}

pub enum TabKind {
    /// 一个桶。
    Browser(BrowserTab),
    /// 传输队列。
    Transfers,
    /// 设置。
    Settings(SettingsForm),
}

impl Tab {
    pub fn browser(&self) -> Option<&BrowserTab> {
        match &self.kind {
            TabKind::Browser(tab) => Some(tab),
            _ => None,
        }
    }

    pub fn browser_mut(&mut self) -> Option<&mut BrowserTab> {
        match &mut self.kind {
            TabKind::Browser(tab) => Some(tab),
            _ => None,
        }
    }

    /// 给落点规则看的样子。
    pub fn slot(&self, cx: &App) -> Slot {
        match self.browser().and_then(|tab| tab.location(cx)) {
            Some(location) => Slot::Bucket(location),
            None if self.browser().is_some() => Slot::Blank,
            None => Slot::Other,
        }
    }

    /// 标签上的字。
    pub fn title(&self, cx: &App) -> SharedString {
        match &self.kind {
            TabKind::Browser(tab) => tab.title(cx),
            TabKind::Transfers => "传输".into(),
            TabKind::Settings(_) => "设置".into(),
        }
    }

    pub fn icon(&self) -> IconName {
        match self.kind {
            TabKind::Browser(_) => IconName::Folder,
            TabKind::Transfers => IconName::ArrowUp,
            TabKind::Settings(_) => IconName::Settings,
        }
    }
}

pub struct BrowserTab {
    /// 稳定的标识。异步请求回来时按它找页——索引会因为关页而挪位。
    pub id: u64,
    pub browser: Entity<BrowserState>,
    pub table: Entity<TableState<FileTable>>,
    pub search: Entity<InputState>,
    /// 目录加载的代号，导航离开后旧请求回来时据此丢弃。
    pub generation: u64,
    /// 翻页请求在途。图标视图靠滚动位置触发翻页，没这个会连发好几次。
    pub paging: bool,
    /// 属性面板里那些要再发一次请求才知道的字段。
    pub extra: InspectorExtra,
    /// 图标视图滚到哪儿了。一页一个，不然切回来位置是别人的。
    /// 列表视图不用管，表格自己记着。
    pub grid_scroll: VirtualListScrollHandle,
}

impl BrowserTab {
    pub fn new(
        id: u64,
        view: ViewMode,
        folders_first: bool,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Self {
        let browser = cx.new(|_| BrowserState::new(view, folders_first));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索当前目录"));
        let delegate = FileTable::new(browser.clone(), cx.entity().downgrade());
        let table = cx.new(|cx| TableState::new(delegate, window, cx));

        // 每页一个搜索框，所以订阅也是每页一份。按 id 找回自己那页：
        // 直接改"当前页"的话，后台页的输入会改到别人头上。
        cx.subscribe(&search, move |this, search, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let value = search.read(cx).value().to_string();
            let Some(tab) = this.tab_by_id(id) else {
                return;
            };
            let (browser, table) = (tab.browser.clone(), tab.table.clone());
            browser.update(cx, |state, cx| {
                state.set_filter(value);
                cx.notify();
            });
            table.update(cx, |_, cx| cx.notify());
        })
        .detach();

        BrowserTab {
            id,
            browser,
            table,
            search,
            generation: 0,
            paging: false,
            extra: InspectorExtra::default(),
            grid_scroll: VirtualListScrollHandle::new(),
        }
    }

    /// 这一页当前在哪儿。还没选桶就是 `None`。
    pub fn location(&self, cx: &App) -> Option<Location> {
        self.browser.read(cx).location().cloned()
    }

    /// 桶名；还没选桶就是"新标签页"。
    fn title(&self, cx: &App) -> SharedString {
        match self.browser.read(cx).location() {
            Some(location) => location.bucket.clone().into(),
            None => "新标签页".into(),
        }
    }
}
