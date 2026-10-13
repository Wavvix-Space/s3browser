//! 标签条：开、切、关，以及它自己怎么画。
//!
//! 打开的桶一格，传输和设置各一格，全都能关，谁也不常驻。文件操作认的
//! "当前桶"单独用 `browsing_id` 记着——站到传输或设置格上时它不动，
//! 切回来还是刚才那个桶。`tabs` 里永远至少留一格浏览页，所以
//! `browsing()` 总有东西可给。

use gpui_kit::component::button::*;
use gpui_kit::component::input::InputState;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::table::TableState;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use s3browser::browser_state::{BrowserState, Location};
use s3browser::tabs::{CloseScope, Slot, TabTarget, active_after_close, pick_tab, tabs_to_close};

use crate::views::browser_tab::{BrowserTab, Tab, TabKind};
use crate::views::list_view::FileTable;
use crate::views::main_window::MainWindow;
use crate::views::menus::tab_menu;
use crate::views::settings::SettingsForm;

/// 标签条的高度。
const TAB_HEIGHT: f32 = 36.0;
/// 一格最宽这么多，桶名再长也不让它把别的挤没。
const TAB_MAX_WIDTH: f32 = 220.0;

impl MainWindow {
    /// 当前这一格。
    pub fn tab(&self) -> &Tab {
        debug_assert!(!self.tabs.is_empty(), "标签条不能一格都不剩");
        &self.tabs[self.active.min(self.tabs.len() - 1)]
    }

    /// 文件操作作用在哪个桶上。
    ///
    /// 当前格是传输或设置时，给的是最近看的那个桶——文件操作那会儿本来就
    /// 不该触发（动作只在浏览格上挂），但取值得有个确定的答案。
    /// `tabs` 里永远至少留一格浏览页，所以这里总有东西可给。
    pub fn browsing(&self) -> &BrowserTab {
        self.tabs
            .iter()
            .find(|t| t.id == self.browsing_id)
            .and_then(Tab::browser)
            .or_else(|| self.tabs.iter().find_map(Tab::browser))
            // 一格浏览页都没开（只剩传输或设置）时顶上空白那份
            .unwrap_or(&self.blank)
    }

    pub fn browsing_mut(&mut self) -> &mut BrowserTab {
        let id = self.browsing_id;
        let found = self
            .tabs
            .iter()
            .position(|t| t.id == id && t.browser().is_some())
            .or_else(|| self.tabs.iter().position(|t| t.browser().is_some()));
        match found {
            Some(ix) => self.tabs[ix].browser_mut().expect("刚挑的就是浏览格"),
            None => &mut self.blank,
        }
    }

    /// 让"文件操作作用在哪个桶上"跟上当前这一格。
    ///
    /// 站在传输或设置格上时不动——那两格没有桶，动了切回来就找不着
    /// 刚才那个了。写成一个有名字的方法是因为这里踩过坑：原来是
    /// `self.browsing_id = self.browsing().id`，而 `browsing()` 本来就是
    /// 按 `browsing_id` 查的，等于把自己赋给自己，一行白写。
    fn follow_active_tab(&mut self) {
        let tab = self.tab();
        if let Some(id) = tab.browser().map(|_| tab.id) {
            self.browsing_id = id;
        }
    }

    /// 按 id 找浏览格。异步回调只能用这个——它跑回来时索引可能已经挪了。
    pub fn tab_by_id(&mut self, id: u64) -> Option<&mut BrowserTab> {
        self.tabs
            .iter_mut()
            .find(|t| t.id == id)
            .and_then(Tab::browser_mut)
    }

    /// 当前格是不是浏览页。文件操作和属性面板据此决定要不要出现。
    pub fn on_browser_tab(&self) -> bool {
        self.tab().browser().is_some()
    }

    pub fn browser(&self) -> &Entity<BrowserState> {
        &self.browsing().browser
    }

    pub fn table(&self) -> &Entity<TableState<FileTable>> {
        &self.browsing().table
    }

    pub fn search(&self) -> &Entity<InputState> {
        &self.browsing().search
    }

    fn next_id(&mut self) -> u64 {
        self.next_tab_id += 1;
        self.next_tab_id - 1
    }

    fn new_browser_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Tab {
        let ui = self.app.read(cx).config.ui.clone();
        let id = self.next_id();
        Tab {
            id,
            kind: TabKind::Browser(BrowserTab::new(id, ui.view, ui.folders_first, window, cx)),
        }
    }

    /// 点了侧边栏的一个桶。开着就切过去，没开就找地方开。
    pub fn open_bucket(&mut self, location: Location, window: &mut Window, cx: &mut Context<Self>) {
        let slots: Vec<Slot> = self.tabs.iter().map(|t| t.slot(cx)).collect();
        match pick_tab(&slots, self.active, &location) {
            TabTarget::Focus(ix) => {
                self.select_tab(ix, cx);
                return;
            }
            TabTarget::Reuse(ix) => self.active = ix,
            TabTarget::New => {
                let tab = self.new_browser_tab(window, cx);
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
            }
        }
        self.follow_active_tab();
        self.open_location(location, window, cx);
    }

    fn find_tab(&self, is_kind: impl Fn(&TabKind) -> bool) -> Option<usize> {
        self.tabs.iter().position(|t| is_kind(&t.kind))
    }

    /// 新开一格并切过去。
    fn push_tab(&mut self, kind: TabKind, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.tabs.push(Tab { id, kind });
        self.active = self.tabs.len() - 1;
        self.preview = None;
        cx.notify();
    }

    /// 当前设置格里那几个输入框。没开设置格时是 None。
    pub fn settings_form(&self) -> Option<&SettingsForm> {
        self.tabs.iter().find_map(|t| match &t.kind {
            TabKind::Settings(form) => Some(form),
            _ => None,
        })
    }

    /// 切到第 `ix` 格。
    pub fn select_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() || ix == self.active {
            return;
        }
        self.active = ix;
        self.follow_active_tab();
        // 换格等于换了一份选中项，浮层和属性面板里的东西全过期了
        self.preview = None;
        self.refresh_table(cx);
        self.refresh_inspector(cx);
        cx.notify();
    }

    /// 关掉第 `ix` 格。
    pub fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        let next = active_after_close(self.tabs.len(), self.active, ix);
        self.tabs.remove(ix);
        self.settle_tabs(next, window, cx);
    }

    /// 右键菜单里的"关闭"。关的是右键点中的那一格，不一定是当前格。
    pub(crate) fn close_context_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.context_tab_index() else {
            return;
        };
        self.close_tab(ix, window, cx);
    }

    /// 右键菜单里那几个"批量关"：关其他、关左边、关右边、全部关。
    pub(crate) fn close_tabs(
        &mut self,
        scope: CloseScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.context_tab_index() else {
            return;
        };
        let id = self.tabs[target].id;
        // 倒序给的，照着删索引才一路有效
        for ix in tabs_to_close(self.tabs.len(), target, scope) {
            self.tabs.remove(ix);
        }
        // 留下的那格顶上来当前格。全部关掉时它也没了，落在补出来的空白页上。
        let active = self.tabs.iter().position(|t| t.id == id).unwrap_or(0);
        self.settle_tabs(active, window, cx);
    }

    /// 右键点中的是第几格。菜单项不带参数，只能开菜单时先记 id，这会儿再找回来。
    /// 取走而不是借用：这个 id 就是为这一次菜单记的，用完不留。
    fn context_tab_index(&mut self) -> Option<usize> {
        let id = self.context_tab.take()?;
        self.tabs.iter().position(|t| t.id == id)
    }

    /// 关掉一批之后的收尾。
    ///
    /// 全关光了才补一张空白页。只要还剩别的格（传输、设置）就让浏览页空着——
    /// 原来只要没有浏览页就补一张，用户关最后那个"新标签页"时它立刻又冒出来，
    /// 看着像关不掉。
    fn settle_tabs(&mut self, active: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            let tab = self.new_browser_tab(window, cx);
            self.tabs.push(tab);
        }
        self.active = active.min(self.tabs.len() - 1);
        self.follow_active_tab();
        self.preview = None;
        self.refresh_table(cx);
        self.refresh_inspector(cx);
        cx.notify();
    }

    /// Ctrl+W：关当前这格。
    pub fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab(self.active, window, cx);
    }

    /// Ctrl+J：打开传输那一格。
    pub fn show_transfers(&mut self, cx: &mut Context<Self>) {
        match self.find_tab(|k| matches!(k, TabKind::Transfers)) {
            Some(ix) => self.select_tab(ix, cx),
            None => self.open_side_tab(TabKind::Transfers, cx),
        }
    }

    /// Ctrl+,：打开设置那一格。
    pub fn show_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.find_tab(|k| matches!(k, TabKind::Settings(_))) {
            self.select_tab(ix, cx);
            return;
        }
        let config = self.app.read(cx).config.clone();
        let form = SettingsForm::new(&config.transfer, &config.edit, window, cx);
        self.open_side_tab(TabKind::Settings(form), cx);
    }

    /// 开一格传输或设置。当前站在空白浏览页上就顶掉它——那一格本来就没东西，
    /// 留着只是白占一个位置。
    ///
    /// 规则和点桶时复用空白页一致（`pick_tab`）：只认当前这页的空白，不去别处抢，
    /// 免得标签顺序跳来跳去。顶掉之后那一格换个新 id：它不再是浏览页了，
    /// `browsing_id` 不该还认着它——认不到会自己退回别的浏览页或那份空白状态。
    fn open_side_tab(&mut self, kind: TabKind, cx: &mut Context<Self>) {
        if self.tabs.get(self.active).map(|t| t.slot(cx)) != Some(Slot::Blank) {
            self.push_tab(kind, cx);
            return;
        }
        let id = self.next_id();
        self.tabs[self.active] = Tab { id, kind };
        self.preview = None;
        self.refresh_table(cx);
        self.refresh_inspector(cx);
        cx.notify();
    }

    /// 整条标签条只剩一格空白浏览页。这时候关了也是白关——立刻补一张
    /// 一模一样的，所以那个 × 和菜单里的"关闭"都不给点。
    fn lone_blank(&self, cx: &App) -> bool {
        self.tabs.len() == 1 && self.tabs[0].slot(cx) == Slot::Blank
    }

    /// 标签条上的一格。
    ///
    /// 自己画，不用 gpui-component 的 `TabBar`——那是个分段控件（设置页那种
    /// 切换器）：标签间距对 underline 变体写死 16px、标题居中、没有关闭按钮
    /// 的概念。文档标签条要的是另一套：彼此紧挨、各自带 ×、超出了横向滚。
    /// 拿前者凑后者，间距抠不动，硬用负外边距又会把首个标签拽出容器裁掉图标。
    #[allow(clippy::too_many_arguments)]
    fn render_tab(
        &self,
        ix: usize,
        id: u64,
        label: String,
        icon: IconName,
        selected: bool,
        lone_blank: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .id(("tab", id as usize))
            .h(px(TAB_HEIGHT))
            .px_3()
            .gap_2()
            .items_center()
            .flex_shrink_0()
            // 桶名可以很长，一格封顶，超了让标题自己省略
            .max_w(px(TAB_MAX_WIDTH))
            .border_b_2()
            // 选中那条 2px 的线要盖住整条标签条底下那道 1px 的线，
            // 不然看着是上下两条
            .mb(px(-1.))
            .border_color(if selected {
                theme.primary
            } else {
                theme.transparent
            })
            .text_color(if selected {
                theme.foreground
            } else {
                theme.muted_foreground
            })
            .when(!selected, |this| {
                this.hover(|this| this.bg(theme.tokens.secondary_hover))
            })
            .child(Icon::new(icon).size_4().flex_shrink_0())
            .child(
                // 别加 flex_1：flex-basis 0 在这种按内容定宽的父容器里会被解成
                // max-content，标题框比文字宽出一大截，多出来的空白全堆在 ×
                // 左边，看着就是"标签之间隔老远"。默认的 flex: 0 1 auto 才对——
                // 自然宽度，撑到 max_w 了才缩着省略。
                div()
                    .min_w_0()
                    .text_sm()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(label),
            )
            .when(!lone_blank, |this| {
                this.child(
                    Button::new(("close-tab", id as usize))
                        .icon(IconName::Close)
                        .xsmall()
                        .ghost()
                        .flex_shrink_0()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // 拦住冒泡，不然关完还会顺手选中这一格
                            cx.stop_propagation();
                            if let Some(ix) = this.tabs.iter().position(|t| t.id == id) {
                                this.close_tab(ix, window, cx);
                            }
                        })),
                )
            })
            // 右键先把点中的是哪一格记下来。菜单挂在整条标签条上，而菜单项
            // 不带参数，只能这么把对象递过去。挡住冒泡，免得被标签条那句
            // "点在空地上"顺手清掉。
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.context_tab = Some(id);
                    cx.stop_propagation();
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.select_tab(ix, cx)))
            .into_any_element()
    }

    /// 内容区顶上的标签条。打开的桶、传输、设置各占一格，全都能关。
    pub(crate) fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // 传输那一格顺带报个数：有在跑的报在跑的，没有但有失败的报失败。
        // 两个都报标签就太长了。
        let queue = self.queue.read(cx);
        let running = queue.active_count();
        let failed = queue.failed_count();

        let lone_blank = self.lone_blank(cx);
        let active = self.active.min(self.tabs.len().saturating_sub(1));

        let tabs: Vec<AnyElement> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let id = tab.id;
                let mut label = tab.title(cx).to_string();
                if matches!(tab.kind, TabKind::Transfers) {
                    if running > 0 {
                        label = format!("传输 {running}");
                    } else if failed > 0 {
                        label = format!("传输 {failed} 失败");
                    }
                }
                self.render_tab(ix, id, label, tab.icon(), ix == active, lone_blank, cx)
            })
            .collect();

        let main = cx.entity().downgrade();
        div()
            .id("tab-strip")
            .w_full()
            .flex_shrink_0()
            // 没被哪个标签挡下来，说明右键点的是标签条右边那截空地，没有对象
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, _| this.context_tab = None),
            )
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .id("tabs")
                    .w_full()
                    // 标签多了横向滚，不换行也不挤扁
                    .overflow_x_scroll()
                    .children(tabs),
            )
            .context_menu(move |menu, _, cx| {
                // 和内容区那个菜单一样，状态在开菜单这一刻现读：渲染时算好塞进
                // 闭包的话，中间那次 notify 不一定赶在右键之前落到这儿。
                let Some(main) = main.upgrade() else {
                    return menu;
                };
                let this = main.read(cx);
                // 右键落在空地上，或者那一格已经没了：不给菜单
                let Some(ix) = this
                    .context_tab
                    .and_then(|id| this.tabs.iter().position(|t| t.id == id))
                else {
                    return menu;
                };
                tab_menu(menu, ix, this.tabs.len(), !this.lone_blank(cx))
            })
    }
}
