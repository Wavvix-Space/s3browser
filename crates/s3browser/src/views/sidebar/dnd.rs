//! 侧边栏的拖拽排序。
//!
//! 落点是「缝」不是「行」：拖到一行的上半就是插到它前面，下半就是插到它后面。
//! 老版本落在行上、再靠 `to - 1` 反算，等于「换到某个位置」而不是「插进某条缝」，
//! 拖到一半根本看不出会落到哪儿。
//!
//! 分两层：每一行记「鼠标停在我哪一半」，每个域的容器管「拖出这块就收线、
//! 在这块里松手才落下」。GPUI 的 `on_drag_move` 对每一次移动、在每一个挂了它的
//! 元素上都会触发，事件里带的是那个元素自己的 bounds——所以行只在鼠标落在自己身上时写，
//! 容器只在鼠标出了自己时清。在某一行上必然也在它的容器里，两个条件互斥，谁先跑都一样。
//!
//! 松手交给容器而不是每行各管各的：插入线就画在行与行之间的缝里，在线上松手也得算数。

use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::views::main_window::MainWindow;

/// 插入线的粗细。
const SLOT_LINE: f32 = 2.;

/// 拖账户时带着的东西。
#[derive(Clone)]
pub struct DraggedProfile {
    pub id: String,
}

/// 拖收藏项时带着的东西。
#[derive(Clone)]
pub struct DraggedFavorite {
    pub index: usize,
}

/// 拖桶时带着的东西。桶只能在自己账户内排序，所以要带账户 id。
#[derive(Clone)]
pub struct DraggedBucket {
    pub profile: String,
    pub key: String,
}

/// 三个互不相干的排序域。缝的编号只在域内有意义。
///
/// 收藏、账户、桶三种载荷类型不同，`on_drop::<T>` 按类型分发，天然收不到别的域的东西。
/// 可同样是桶，不同账户之间类型一样，只能靠这里带着的账户 id 分开。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DropDomain {
    Favorites,
    Profiles,
    Buckets(String),
}

impl MainWindow {
    /// 拖拽经过某一行时记下落点。`ix` 是这一行在自己域里的序号。
    pub(crate) fn track_drop_slot<T: 'static>(
        &mut self,
        domain: DropDomain,
        ix: usize,
        event: &DragMoveEvent<T>,
        cx: &mut Context<Self>,
    ) {
        let bounds = event.bounds;
        let position = event.event.position;
        if !bounds.contains(&position) {
            return;
        }
        // 上半是这一行前面那条缝，下半是后面那条
        let slot = if position.y < bounds.center().y {
            ix
        } else {
            ix + 1
        };
        self.set_drop_slot(Some((domain, slot)), cx);
    }

    /// 拖出一个域的范围就收起它的线。挂在域的容器上。
    ///
    /// 不收的话，线会停在最后经过的那条缝上，而这时松手根本落不下去——
    /// 看着能放、放了没反应，比没有线还糟。
    pub(crate) fn leave_drop_zone<T: 'static>(
        &mut self,
        domain: &DropDomain,
        event: &DragMoveEvent<T>,
        cx: &mut Context<Self>,
    ) {
        if event.bounds.contains(&event.event.position) {
            return;
        }
        if self.drop_slot.as_ref().is_some_and(|(d, _)| d == domain) {
            self.set_drop_slot(None, cx);
        }
    }

    fn set_drop_slot(&mut self, next: Option<(DropDomain, usize)>, cx: &mut Context<Self>) {
        // 鼠标每动一下都会进来，真变了才重画
        if self.drop_slot != next {
            self.drop_slot = next;
            cx.notify();
        }
    }

    /// 这条缝上要不要画线。
    ///
    /// 在侧边栏外面松手时拖拽直接取消，取消没有任何回调——`drop_slot` 可能留着旧值。
    /// 所以这里查一次 `has_active_drag`：没有拖拽在进行就不画，状态残留也看不见。
    pub(crate) fn shows_slot(&self, domain: &DropDomain, slot: usize, cx: &App) -> bool {
        cx.has_active_drag()
            && self
                .drop_slot
                .as_ref()
                .is_some_and(|(d, s)| d == domain && *s == slot)
    }

    /// 取出落点并清空。落下时调一次。
    pub(crate) fn take_drop_slot(&mut self, domain: &DropDomain) -> Option<usize> {
        match self.drop_slot.take() {
            Some((d, slot)) if &d == domain => Some(slot),
            _ => None,
        }
    }
}

/// 给一行（或一整块）套上插入线：上面一条；是域里最后一个的话，下面再来一条。
///
/// 线是常驻的 2px 高的 div，平时透明，轮到它才染色——出现和消失都不会让行跳动，
/// 也正好顶替了行与行之间原来那 2px 的间距。没用边框：这版 gpui 的 `border_color`
/// 一次管四条边，染不了单独一条。颜色用 `drag_border`，gpui-component 自己的表格
/// 列拖拽也拿它画线；`accent` 是悬停底色，画成细线根本看不见。
pub(crate) fn with_slot_lines(
    element: impl IntoElement,
    main: &MainWindow,
    domain: &DropDomain,
    ix: usize,
    is_last: bool,
    cx: &App,
) -> Div {
    let color = cx.theme().drag_border;
    let line = |shown: bool| {
        div()
            .h(px(SLOT_LINE))
            .flex_shrink_0()
            .rounded_full()
            .when(shown, |this| this.bg(color))
    };
    v_flex()
        .w_full()
        .flex_shrink_0()
        .child(line(main.shows_slot(domain, ix, cx)))
        .child(element)
        .when(is_last, |this| {
            this.child(line(main.shows_slot(domain, ix + 1, cx)))
        })
}
