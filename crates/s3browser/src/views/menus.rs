//! 右键菜单。列表行、图标单元格、空白处共用这一套，免得三处菜单项各长各的。

use gpui_kit::component::menu::PopupMenu;

use crate::{
    CloseAllTabs, CloseOtherTabs, CloseTabsLeft, CloseTabsRight, CloseThisTab, CopyItems, CopyKey,
    CutItems, DeleteSelected, Download, EditFile, FavoriteEntry, NewFolder, OpenEntry, PasteItems,
    Refresh, Rename, ShareLink, ToggleFavorite, ToggleInspector, UploadFiles, UploadFolder,
};

/// 右键落在条目上时的菜单。
///
/// `single` 决定重命名能不能点——重命名一次只能改一个。`folder_favorited` 是选中的
/// 那个文件夹收藏了没，决定收藏那一项是「收藏」还是「取消收藏」。
pub fn entry_menu(
    menu: PopupMenu,
    single: bool,
    can_paste: bool,
    folder_favorited: bool,
) -> PopupMenu {
    menu.menu("打开", Box::new(OpenEntry))
        // 能不能编辑要看后缀在不在名单里，这儿不判——菜单项不带参数，
        // 判了也只能灰着；不如点一下，由它告诉你"去设置里加"
        .menu_with_disabled("编辑", Box::new(EditFile), !single)
        .menu("属性", Box::new(ToggleInspector))
        // 选中的是文件时动作那边会提示「只能收藏文件夹」，菜单项不带参数，这儿判不了
        .menu(
            if folder_favorited {
                "取消收藏这个文件夹"
            } else {
                "收藏这个文件夹"
            },
            Box::new(FavoriteEntry),
        )
        .separator()
        .menu("下载", Box::new(Download))
        .menu("分享链接", Box::new(ShareLink))
        .menu("复制 key", Box::new(CopyKey))
        .separator()
        .menu_with_disabled("重命名", Box::new(Rename), !single)
        .menu("复制", Box::new(CopyItems))
        .menu("剪切", Box::new(CutItems))
        .menu_with_disabled("粘贴", Box::new(PasteItems), !can_paste)
        .separator()
        .menu("删除", Box::new(DeleteSelected))
}

/// 右键落在空白处时的菜单：只放针对当前目录的操作。`favorited` 是当前目录收藏了没。
pub fn directory_menu(menu: PopupMenu, can_paste: bool, favorited: bool) -> PopupMenu {
    menu.menu("新建文件夹", Box::new(NewFolder))
        .menu(
            if favorited {
                "取消收藏此目录"
            } else {
                "收藏此目录"
            },
            Box::new(ToggleFavorite),
        )
        .menu_with_disabled("粘贴", Box::new(PasteItems), !can_paste)
        .separator()
        .menu("上传文件", Box::new(UploadFiles))
        .menu("上传文件夹", Box::new(UploadFolder))
        .separator()
        .menu("刷新", Box::new(Refresh))
}

/// 右键落在标签上时的菜单。
///
/// `ix` 是这一格在标签条上的位置，`count` 是总共几格——点了也不会有动静的
/// 项按这两个数灰掉，省得用户以为坏了。`can_close` 管的是另一回事：整条
/// 标签条只剩一格空白浏览页时关了也白关，立刻又补一张一样的。
pub fn tab_menu(menu: PopupMenu, ix: usize, count: usize, can_close: bool) -> PopupMenu {
    menu.menu_with_disabled("关闭", Box::new(CloseThisTab), !can_close)
        .menu_with_disabled("关闭其他标签页", Box::new(CloseOtherTabs), count < 2)
        .separator()
        .menu_with_disabled("关闭左侧标签页", Box::new(CloseTabsLeft), ix == 0)
        .menu_with_disabled("关闭右侧标签页", Box::new(CloseTabsRight), ix + 1 >= count)
        .separator()
        .menu_with_disabled("全部关闭", Box::new(CloseAllTabs), !can_close)
}
