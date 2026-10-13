#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// gpui-kit 的元素构建器链很长，测试展开时会超过默认的宏递归上限
#![recursion_limit = "256"]

mod app_state;
mod views;

use s3browser::{config, logging};

// 默认那 101 个图标里只有 file / file-text / folder 三个和条目沾边，
// 分不出图片、压缩包、代码。全集是 1830 个 SVG，一共 750 KB，
// 换来每种文件一个图形。
use gpui_kit::assets::AllAssets;
use gpui_kit::component::scroll::ScrollbarMode;
use gpui_kit::component::*;
use gpui_kit::*;

use views::main_window::{MainWindow, STATUS_BAR_HEIGHT};

gpui_kit::actions!(
    s3browser,
    [
        NewFolder,
        Rename,
        DeleteSelected,
        CopyItems,
        CutItems,
        PasteItems,
        Refresh,
        GoUp,
        GoBack,
        GoForward,
        GoTo,
        ShareLink,
        EditAccount,
        RemoveAccount,
        RefreshBuckets,
        OpenEntry,
        TogglePreview,
        ClosePreview,
        ToggleInspector,
        ViewList,
        ViewIcon,
        UploadFiles,
        UploadFolder,
        Download,
        CopyKey,
        EditFile,
        FocusSearch,
        ShowTransfers,
        CloseTab,
        // 标签条右键菜单里的那几件事。关的是右键点中的那一格，
        // 不是当前格，所以和 Ctrl+W 的 CloseTab 分开。
        CloseThisTab,
        CloseOtherTabs,
        CloseTabsLeft,
        CloseTabsRight,
        CloseAllTabs,
        OpenSettings,
        // 侧边栏的收藏。菜单项不带参数，作用在谁身上看 MainWindow 里记下的
        // context_bucket / context_favorite
        ToggleFavorite,
        FavoriteEntry,
        FavoriteBucket,
        OpenFavorite,
        RenameFavorite,
        RemoveFavorite,
        // 桶的本地隐藏
        HideBucket,
        OnlyShowBucket,
        ToggleShowHidden,
        UnhideAll,
        SelectAll,
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
    ]
);

/// 列表区的按键上下文。输入框聚焦时不会走到这里。
pub const BROWSER_CONTEXT: &str = "Browser";

/// 窗口能缩到多小。侧边栏 220 加属性面板 260 就占掉 480，
/// 再窄下去内容区只剩个缝，标签条也挤成一团。
const MIN_WINDOW: (f32, f32) = (900.0, 600.0);

/// 键位表，对应设计文档 8.7。输入框聚焦时是 `Input` 上下文，不会落到这里。
fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // 打开与编辑
        KeyBinding::new("enter", OpenEntry, Some(BROWSER_CONTEXT)),
        KeyBinding::new("f2", Rename, Some(BROWSER_CONTEXT)),
        KeyBinding::new("delete", DeleteSelected, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-shift-n", NewFolder, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-c", CopyItems, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-x", CutItems, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-v", PasteItems, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(BROWSER_CONTEXT)),
        // 导航
        KeyBinding::new("backspace", GoUp, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-up", GoUp, Some(BROWSER_CONTEXT)),
        KeyBinding::new("alt-left", GoBack, Some(BROWSER_CONTEXT)),
        KeyBinding::new("alt-right", GoForward, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-[", GoBack, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-]", GoForward, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-shift-g", GoTo, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-r", Refresh, Some(BROWSER_CONTEXT)),
        KeyBinding::new("f5", Refresh, Some(BROWSER_CONTEXT)),
        // 选择。上下在图标视图里跳一整行，由动作处理时按列数换算
        KeyBinding::new("up", MoveUp, Some(BROWSER_CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(BROWSER_CONTEXT)),
        KeyBinding::new("left", MoveLeft, Some(BROWSER_CONTEXT)),
        KeyBinding::new("right", MoveRight, Some(BROWSER_CONTEXT)),
        // 视图
        KeyBinding::new("ctrl-1", ViewIcon, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-2", ViewList, Some(BROWSER_CONTEXT)),
        KeyBinding::new("space", TogglePreview, Some(BROWSER_CONTEXT)),
        KeyBinding::new("escape", ClosePreview, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-i", ToggleInspector, Some(BROWSER_CONTEXT)),
        KeyBinding::new("alt-enter", ToggleInspector, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-f", FocusSearch, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-j", ShowTransfers, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-w", CloseTab, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-,", OpenSettings, Some(BROWSER_CONTEXT)),
        // 传输与分享
        KeyBinding::new("ctrl-u", UploadFiles, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-shift-u", UploadFolder, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-d", Download, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-shift-s", ShareLink, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-shift-c", CopyKey, Some(BROWSER_CONTEXT)),
        KeyBinding::new("ctrl-e", EditFile, Some(BROWSER_CONTEXT)),
    ]);
}

/// 日志默认只开 info，想看细节设 `S3B_LOG=debug`。
/// 调试构建打到 stderr，release 下没有控制台，所以写到数据目录的 logs 里。
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("S3B_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,s3browser=debug,transfer=debug"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);

    if cfg!(debug_assertions) {
        builder.with_writer(std::io::stderr).init();
    } else {
        let path = config::data_dir().join("logs").join("s3browser.log");
        // 5 MB 一滚，留 3 份历史，免得跑久了把盘写满
        if let Ok(file) = logging::RollingFile::new(path) {
            builder
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
            return;
        }
        builder.with_writer(std::io::stderr).init();
    }
}

/// 换配色，顺带把我们自己那几项设置重新贴回去。
///
/// `Theme::change` 把整个主题连同 gpui-base 那层一起重建，通知位置和滚动条
/// 模式都是挂在上面的，不重贴一遍就被冲掉了——换一次深色，滚动条就变回
/// 「滚的时候才出现」。所以换色和这几项必须绑在一块，别处不要单独调 change。
pub fn apply_theme(choice: config::Theme, window: Option<&mut Window>, cx: &mut App) {
    match choice {
        config::Theme::System => Theme::sync_system_appearance(window, cx),
        config::Theme::Light => Theme::change(ThemeMode::Light, window, cx),
        config::Theme::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }

    // 滚动条常显。默认是「滚的时候才出现，停两秒淡出」，在文件列表里这跟没有
    // 差不多——一屏装不下时没有任何东西告诉你下面还有。
    Theme::set_scrollbar_mode(ScrollbarMode::Always, cx);

    // 提示挪到右下角，和状态栏错开。默认是右上角，那儿正好压着搜索框。
    let theme = Theme::global_mut(cx);
    theme.notification.placement = Anchor::BottomRight;
    theme.notification.margins.bottom = px(STATUS_BAR_HEIGHT + 12.);
    theme.notification.width = px(320.);
}

fn main() {
    init_logging();
    // 上次「立刻更新」换下来的旧程序，换的那会儿它自己正跑着删不掉，现在补一刀
    s3browser::update::sweep_old();
    let app = gpui_kit::application().with_assets(AllAssets);

    app.run(|cx| {
        gpui_kit::init(cx);
        bind_keys(cx);

        let ui = config::Config::load(&config::config_path()).ui;
        apply_theme(ui.theme, None, cx);

        let size = ui.window;
        let bounds = Bounds::centered(None, gpui::size(px(size.width), px(size.height)), cx);

        cx.spawn(async move |cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // 再小就没法用了：侧边栏和属性面板是定宽的，内容区会被挤没
                window_min_size: Some(gpui::size(px(MIN_WINDOW.0), px(MIN_WINDOW.1))),
                titlebar: Some(TitlebarOptions {
                    title: Some("S3Browser".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };

            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| MainWindow::new(window, cx));
                // 用的是系统标题栏，不要 Root 默认的自绘窗口边框：
                // 那层阴影边距会占掉宽高，把右侧和底部的控件挤出可见区。
                let root = cx.new(|cx| {
                    Root::new(view.clone(), window, cx)
                        .bordered(false)
                        .bg(cx.theme().background)
                });
                // 启动检查放在窗口和 Root 都建好之后再发起。在 MainWindow::new
                // 中途发起的话，回来的提示推进通知列表是成功的，但那一帧画不出来。
                view.update(cx, |this, cx| {
                    if this.app.read(cx).config.update.check_on_startup {
                        this.check_updates(false, cx);
                    }
                });
                root
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
