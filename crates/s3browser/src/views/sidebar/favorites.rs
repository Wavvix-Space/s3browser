//! 收藏区：跨账户的常用位置，排在侧边栏最上面。
//!
//! 收藏的是一个完整的 `Location`（桶，或桶里的某一层目录），不经过 `BucketView`：
//! 显式收藏过的东西，不该因为桶被藏起来就跟着消失。

use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::dnd::{DraggedFavorite, DropDomain, with_slot_lines};
use super::{ROW_HEIGHT, hint_row};
use crate::views::dialogs::InputPurpose;
use crate::views::drag::DragLabel;
use crate::views::main_window::MainWindow;
use crate::{OpenFavorite, RemoveFavorite, RenameFavorite};
use s3browser::config::Favorite;
use s3browser::location::Location;

impl MainWindow {
    /// 收藏区的所有行。收起来时返回空；一条都没有时给句提示。
    pub(super) fn render_favorites(
        &self,
        current: Option<&Location>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (collapsed, favorites) = {
            let app = self.app.read(cx);
            (
                app.config.ui.favorites_collapsed,
                app.config.favorites.clone(),
            )
        };
        if collapsed {
            return Vec::new();
        }
        if favorites.is_empty() {
            // 不给提示的话，没人知道这儿能放东西
            return vec![hint_row("右键桶，或在工具栏点 ☆", cx)];
        }
        favorites
            .iter()
            .enumerate()
            .map(|(ix, fav)| self.render_favorite(ix, favorites.len(), fav, current, cx))
            .collect()
    }

    fn render_favorite(
        &self,
        ix: usize,
        total: usize,
        fav: &Favorite,
        current: Option<&Location>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 和星标判「已收藏」用同一把尺子：只比账户、桶、目录。
        // 目录也在比较之列，所以从收藏跳进去再往下走一层，就不再是「在这儿」了
        let active = current.is_some_and(|c| fav.same_place(c));
        let account = self
            .app
            .read(cx)
            .config
            .profile(&fav.at.profile)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        // 桶根和桶里的目录用不同图标，扫一眼就知道点进去是哪一层
        let icon = if fav.is_bucket_root() {
            IconName::HardDrive
        } else {
            IconName::FolderClosed
        };
        // 名字能改、账户名会截断，悬停时说清它到底指向哪儿
        let full_path = format!("{account} · {}", fav.at.segments().join("/"));

        let theme = cx.theme();
        let row = h_flex()
            .id(("favorite", ix))
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
            .when(active, |this| {
                this.font_medium()
                    .bg(theme.tokens.sidebar_accent)
                    .text_color(theme.sidebar_accent_foreground)
            })
            .when(!active, |this| {
                this.hover(|this| this.bg(theme.sidebar_accent.opacity(0.8)))
            })
            .on_click(cx.listener({
                let at = fav.at.clone();
                move |this, _, window, cx| this.open_favorite(at.clone(), window, cx)
            }))
            .tooltip(move |window, cx| Tooltip::new(full_path.clone()).build(window, cx))
            .on_drag(DraggedFavorite { index: ix }, {
                let label = fav.name.clone();
                move |_, _, _, cx| cx.new(|_| DragLabel::new(label.clone()))
            })
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<DraggedFavorite>, _, cx| {
                    this.track_drop_slot(DropDomain::Favorites, ix, event, cx);
                },
            ))
            .child(
                Icon::new(icon)
                    .size_4()
                    .flex_shrink_0()
                    .text_color(theme.muted_foreground),
            )
            .child(div().flex_1().min_w_0().truncate().child(fav.name.clone()))
            // 哪个账户的，靠这行灰字区分同名的桶
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px(64.))
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(account),
            )
            // 菜单项不带参数，先记下右键的是第几条。记索引没问题：收藏只会因为
            // 用户操作而变，菜单开着的时候点不到别处。放在链子最后：
            // context_menu 会把元素包起来，之后接不上别的了
            .context_menu({
                let main = cx.entity().downgrade();
                move |menu, _, cx| {
                    main.update(cx, |this, _| this.context_favorite = Some(ix))
                        .ok();
                    menu.menu("打开", Box::new(OpenFavorite))
                        .separator()
                        .menu("重命名", Box::new(RenameFavorite))
                        .menu("取消收藏", Box::new(RemoveFavorite))
                }
            });

        with_slot_lines(row, self, &DropDomain::Favorites, ix, ix + 1 == total, cx)
            .into_any_element()
    }

    /// 打开一条收藏：切到那个桶的标签页（没开就开），再走到收藏的那一层。
    ///
    /// 和点侧边栏的桶不一样。点桶是「回到那个桶」，桶已经开着的话切过去、停在上次看到的
    /// 目录就好；收藏指的是一个具体目录，桶开着也得走过去——不然点了没反应。
    pub fn open_favorite(&mut self, at: Location, window: &mut Window, cx: &mut Context<Self>) {
        self.open_bucket(at.clone(), window, cx);
        let arrived = self
            .browser()
            .read(cx)
            .location()
            .is_some_and(|here| here.bucket == at.bucket && here.prefix == at.prefix);
        if !arrived {
            self.open_location(at, window, cx);
        }
    }

    /// 当前位置收藏了没。工具栏按钮和桶行的星标据此高亮。
    pub fn is_favorited(&self, at: &Location, cx: &App) -> bool {
        self.app.read(cx).config.favorite_index(at).is_some()
    }

    /// 收藏或取消收藏一处位置。
    pub fn toggle_favorite_at(&mut self, at: Location, cx: &mut Context<Self>) {
        let added = self.app.update(cx, |app, cx| {
            let added = app.config.toggle_favorite(&at);
            app.save_config();
            cx.notify();
            added
        });
        self.toast_info(if added { "已收藏" } else { "已取消收藏" }, cx);
        cx.notify();
    }

    /// 收藏当前正在看的位置。工具栏按钮和「收藏此目录」都走这儿。
    pub fn toggle_favorite_here(&mut self, cx: &mut Context<Self>) {
        let Some(at) = self.browser().read(cx).location().cloned() else {
            return;
        };
        self.toggle_favorite_at(at, cx);
    }

    /// 收藏选中的那个文件夹。选中的是文件就不干活——文件收藏了也打不开。
    pub fn favorite_selected_entry(&mut self, cx: &mut Context<Self>) {
        // 先把要的东西拷出来，块结束时对 cx 的借用就还回去了，
        // 下面弹提示、改配置才拿得到 &mut
        let (location, folder) = {
            let state = self.browser().read(cx);
            let folder = state
                .selected_entries()
                .into_iter()
                .find(|e| e.is_dir)
                .map(|e| e.name.to_string());
            (state.location().cloned(), folder)
        };
        let Some(location) = location else {
            return;
        };
        let Some(folder) = folder else {
            self.toast_info("只能收藏文件夹", cx);
            return;
        };
        self.toggle_favorite_at(location.child(&folder), cx);
    }

    /// 收藏右键点中的那个桶。
    pub fn favorite_context_bucket(&mut self, cx: &mut Context<Self>) {
        let Some((at, _)) = self.context_bucket.clone() else {
            return;
        };
        self.toggle_favorite_at(at, cx);
    }

    /// 打开右键点中的那条收藏。和左键点一样，菜单里也放一份，给习惯先右键的人。
    pub fn open_context_favorite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.context_favorite else {
            return;
        };
        let Some(at) = self
            .app
            .read(cx)
            .config
            .favorites
            .get(ix)
            .map(|f| f.at.clone())
        else {
            return;
        };
        self.open_favorite(at, window, cx);
    }

    /// 取消右键点中的那条收藏。
    pub fn remove_context_favorite(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.context_favorite.take() else {
            return;
        };
        self.app.update(cx, |app, cx| {
            if ix < app.config.favorites.len() {
                app.config.favorites.remove(ix);
                app.save_config();
            }
            cx.notify();
        });
        cx.notify();
    }

    /// 给右键点中的那条收藏改名。
    pub fn rename_favorite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.context_favorite else {
            return;
        };
        let Some(current) = self
            .app
            .read(cx)
            .config
            .favorites
            .get(index)
            .map(|f| f.name.clone())
        else {
            return;
        };
        self.open_input(
            InputPurpose::RenameFavorite { index },
            "重命名收藏",
            &current,
            window,
            cx,
        );
    }

    /// 输入框点了确定之后真的改。
    ///
    /// 不用自己关对话框：`InputDialog::confirm` 调进来之前就把 `main.dialog` 清了。
    pub(crate) fn apply_favorite_rename(
        &mut self,
        index: usize,
        name: String,
        cx: &mut Context<Self>,
    ) {
        self.app.update(cx, |app, cx| {
            if let Some(fav) = app.config.favorites.get_mut(index) {
                fav.name = name.trim().to_owned();
                app.save_config();
            }
            cx.notify();
        });
        cx.notify();
    }

    /// 收藏拖拽落下：把第 `from` 条挪到当前悬着的那条缝上。
    pub fn drop_favorite(&mut self, from: usize, cx: &mut Context<Self>) {
        let Some(slot) = self.take_drop_slot(&DropDomain::Favorites) else {
            return;
        };
        self.app.update(cx, |app, cx| {
            s3browser::reorder::move_to_slot(&mut app.config.favorites, from, slot);
            app.save_config();
            cx.notify();
        });
        cx.notify();
    }
}
