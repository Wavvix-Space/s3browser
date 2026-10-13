//! 「全部」分组里的两种行：账户，和它展开后底下的桶。

use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::dnd::{DraggedBucket, DraggedProfile, DropDomain, with_slot_lines};
use super::{ROW_HEIGHT, hint_row};
use crate::views::drag::DragLabel;
use crate::views::main_window::MainWindow;
use crate::{
    EditAccount, FavoriteBucket, HideBucket, OnlyShowBucket, RefreshBuckets, RemoveAccount,
    ToggleShowHidden, UnhideAll,
};
use s3browser::browser_state::Location;
use s3browser::buckets::BucketEntry;
use s3browser::config::Profile;

impl MainWindow {
    /// 一个账户：自己那行，加上展开时底下的桶。`total` 是账户总数，最后一块要多画一条缝。
    ///
    /// 拖拽、点击、右键菜单都挂在账户行这一个元素上。分到里外两层去挂的话，
    /// 内层的 hitbox 会把鼠标按下收走，外层的 `on_drag` 就再也等不到了。
    pub(super) fn render_account(
        &self,
        ix: usize,
        total: usize,
        profile: &Profile,
        current: Option<&Location>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = profile.id.clone();
        let (expanded, list, show_hidden) = {
            let app = self.app.read(cx);
            (
                app.is_expanded(&id),
                app.bucket_list(&id),
                app.shows_hidden(&id),
            )
        };
        // 收起着的账户不用排：这里每画一帧都要跑，账户多了白算
        let arranged = if expanded {
            profile.view.arrange(&list.buckets, show_hidden)
        } else {
            Vec::new()
        };

        let buckets: Vec<AnyElement> = if !expanded {
            Vec::new()
        } else if arranged.is_empty() {
            let hint = if list.loading {
                "正在加载…".to_owned()
            } else if let Some(error) = &list.error {
                error.clone()
            } else if !list.buckets.is_empty() {
                // 桶是有的，只是全藏起来了。这时候说"没有桶"会让人以为账户坏了
                "桶都藏起来了".to_owned()
            } else if list.loaded {
                "没有桶".to_owned()
            } else {
                "展开以加载".to_owned()
            };
            vec![hint_row(hint, cx)]
        } else {
            arranged
                .iter()
                .enumerate()
                .map(|(bix, (entry, hidden))| {
                    self.render_bucket(ix, bix, arranged.len(), &id, entry, *hidden, current, cx)
                })
                .collect()
        };

        let theme = cx.theme();
        let label: SharedString = profile.name.clone().into();
        let drag_label = label.clone();
        let toggle_id = id.clone();
        let menu_id = id.clone();

        let account_row = h_flex()
            .id(("account", ix))
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex_shrink_0()
            .px_2()
            .gap_2()
            .items_center()
            .text_sm()
            .rounded(theme.radius)
            .cursor_pointer()
            .overflow_x_hidden()
            .hover(|this| this.bg(theme.sidebar_accent.opacity(0.8)))
            .on_drag(DraggedProfile { id: id.clone() }, move |_, _, _, cx| {
                cx.new(|_| DragLabel::new(drag_label.clone()))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_profile(&toggle_id, cx);
            }))
            .child(Icon::new(IconName::HardDrive).size_4().flex_shrink_0())
            // 名字太长就截断加省略号。不截的话会折成两行，把行高撑破、压到下一行上
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                Icon::new(IconName::ChevronRight)
                    .size_4()
                    .flex_shrink_0()
                    .text_color(theme.muted_foreground)
                    .when(expanded, |this| this.rotate(percentage(90. / 360.))),
            )
            // 菜单项本身不带参数，所以先记下右键的是哪个账户，
            // 动作处理时再取。这个闭包就是在右键那一刻跑的。
            // 放在链子最后：context_menu 会把元素包起来，之后接不上别的了。
            .context_menu({
                let main = cx.entity().downgrade();
                move |menu, _, cx| {
                    let (hidden_count, any_hidden, showing) = main
                        .update(cx, |this, cx| {
                            this.context_profile = Some(menu_id.clone());
                            let app = this.app.read(cx);
                            let view = app
                                .config
                                .profile(&menu_id)
                                .map(|p| p.view.clone())
                                .unwrap_or_default();
                            (
                                view.hidden_count(&app.bucket_list(&menu_id).buckets),
                                !view.hidden.is_empty(),
                                app.shows_hidden(&menu_id),
                            )
                        })
                        .unwrap_or((0, false, false));
                    menu.menu("编辑账户", Box::new(EditAccount))
                        .menu("刷新桶列表", Box::new(RefreshBuckets))
                        .separator()
                        // 一个都没藏、开关又关着时灰掉：点了没反应比灰着更让人困惑。
                        // 开着的时候永远能点，不然把桶全放出来之后就关不掉了
                        .menu_with_check_and_disabled(
                            format!("显示隐藏的桶 ({hidden_count})"),
                            showing,
                            Box::new(ToggleShowHidden),
                            hidden_count == 0 && !showing,
                        )
                        // 这一项按名单本身判：它清掉的正好包括那些已经对不上号的旧 key
                        .menu_with_disabled("取消所有隐藏", Box::new(UnhideAll), !any_hidden)
                        .separator()
                        .menu("删除账户", Box::new(RemoveAccount))
                }
            });

        // 这个账户的桶自成一个落点范围：拖出去线就收起，只有在这里松手才算数。
        // 没有 gap：每行上面都带着一条 2px 的线（平时透明），正好顶替原来的行间距
        let bucket_nest = v_flex()
            .ml_4()
            .pl_1()
            .py_0p5()
            .border_l_1()
            .border_color(theme.sidebar_border)
            .children(buckets)
            .on_drag_move(cx.listener({
                let domain = DropDomain::Buckets(id.clone());
                move |this, event: &DragMoveEvent<DraggedBucket>, _, cx| {
                    this.leave_drop_zone(&domain, event, cx);
                }
            }))
            .on_drop(cx.listener({
                let owner = id.clone();
                move |this, dragged: &DraggedBucket, _, cx| {
                    // 别的账户的桶落不进来
                    if dragged.profile == owner {
                        this.drop_bucket(&owner, &dragged.key, cx);
                    }
                }
            }));

        // 线画在整块上：账户行加上它展开的那些桶，算一个不可分割的单位。
        // 落点也按整块算，所以 on_drag_move 挂在块上而不是账户行上——拖着账户
        // 经过别人展开的桶列表时，线只停在那一块的上下边缘，不会插进人家的桶中间。
        let block = v_flex()
            .child(account_row)
            .when(expanded, |this| this.child(bucket_nest))
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<DraggedProfile>, _, cx| {
                    this.track_drop_slot(DropDomain::Profiles, ix, event, cx);
                },
            ));
        with_slot_lines(block, self, &DropDomain::Profiles, ix, ix + 1 == total, cx)
            .into_any_element()
    }

    /// 账户底下的一个桶。`total` 是这个账户显示出来的桶数。
    #[allow(clippy::too_many_arguments)]
    fn render_bucket(
        &self,
        ix: usize,
        bix: usize,
        total: usize,
        profile_id: &str,
        entry: &BucketEntry,
        hidden: bool,
        current: Option<&Location>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let location = entry.location(profile_id);
        // 在这个桶里的哪一层都算
        let active = current.is_some_and(|l| {
            l.profile == location.profile && l.bucket == location.bucket
        });
        let favorited = self.is_favorited(&location, cx);
        // 顺序和隐藏名单里认它的那个 key。右键、拖拽都把它原样带走，
        // 动作那边不再从 Location 反算——算法只留 `BucketEntry::key` 这一份
        let key = entry.key();
        let domain = DropDomain::Buckets(profile_id.to_owned());

        let theme = cx.theme();
        let row = h_flex()
            .id(SharedString::from(format!("bucket-{ix}-{bix}")))
            // 星标靠它知道鼠标在不在这一行上
            .group("bucket-row")
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex_shrink_0()
            .px_2()
            .gap_1()
            .items_center()
            .text_sm()
            .rounded(theme.radius)
            .cursor_pointer()
            .overflow_x_hidden()
            // 藏起来的桶只在"显示隐藏的桶"开着时出现，画淡一点以示区别
            .when(hidden, |this| this.opacity(0.45))
            .when(active, |this| {
                this.font_medium()
                    .bg(theme.tokens.sidebar_accent)
                    .text_color(theme.sidebar_accent_foreground)
            })
            .when(!active, |this| {
                this.hover(|this| this.bg(theme.sidebar_accent.opacity(0.8)))
            })
            // 一个桶一个标签页：开着就切过去，没开才新开一页
            .on_click(cx.listener({
                let at = location.clone();
                move |this, _, window, cx| this.open_bucket(at.clone(), window, cx)
            }))
            .on_drag(
                DraggedBucket {
                    profile: profile_id.to_owned(),
                    key: key.clone(),
                },
                {
                    let label = entry.bucket.clone();
                    move |_, _, _, cx| cx.new(|_| DragLabel::new(label.clone()))
                },
            )
            .on_drag_move(cx.listener({
                let owner = profile_id.to_owned();
                let domain = domain.clone();
                move |this, event: &DragMoveEvent<DraggedBucket>, _, cx| {
                    // 只在自己账户里排。拖的是别的账户的桶就当没看见——
                    // 那边的线由它自己的桶列表在鼠标离开时收掉
                    if event.drag(cx).profile == owner {
                        this.track_drop_slot(domain.clone(), bix, event, cx);
                    }
                }
            }))
            .child(div().flex_1().min_w_0().truncate().child(entry.bucket.clone()))
            .child(
                div()
                    .id(SharedString::from(format!("fav-{ix}-{bix}")))
                    .flex_shrink_0()
                    .p_0p5()
                    .rounded(theme.radius)
                    .when(!favorited, |this| {
                        // 没收藏的平时不占视线，鼠标划过这一行才浮出来
                        this.opacity(0.)
                            .group_hover("bucket-row", |style| style.opacity(0.55))
                    })
                    .hover(|this| this.opacity(1.))
                    // GPUI 的点击不会自己停在内层：按下时两层都记下、抬起时两层都触发。
                    // 在按下时截住，行就收不到这次按下——点星标不会连带打开桶，
                    // 按住星标拖也不会把整行拖走
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener({
                        let at = location.clone();
                        move |this, _, _, cx| this.toggle_favorite_at(at.clone(), cx)
                    }))
                    // lucide 没有实心星，收没收藏靠颜色分：收了是常显的黄色
                    .child(Icon::new(IconName::Star).size_4().text_color(if favorited {
                        theme.yellow
                    } else {
                        theme.muted_foreground
                    })),
            )
            // 菜单项不带参数，先记下右键的是哪个桶，动作处理时再取。
            // 放在链子最后：context_menu 会把元素包起来，之后接不上别的了
            .context_menu({
                let main = cx.entity().downgrade();
                // 藏着的桶只在「显示隐藏的桶」开着时右键得到，这时它该是一条回头路
                let hide_label = if hidden { "取消隐藏" } else { "隐藏" };
                move |menu, _, cx| {
                    let favorited = main
                        .update(cx, |this, cx| {
                            this.context_bucket = Some((location.clone(), key.clone()));
                            this.is_favorited(&location, cx)
                        })
                        .unwrap_or(false);
                    menu.menu(
                        if favorited { "取消收藏" } else { "收藏" },
                        Box::new(FavoriteBucket),
                    )
                    .separator()
                    .menu(hide_label, Box::new(HideBucket))
                    .menu("只显示这个", Box::new(OnlyShowBucket))
                }
            });

        with_slot_lines(row, self, &domain, bix, bix + 1 == total, cx).into_any_element()
    }
}
