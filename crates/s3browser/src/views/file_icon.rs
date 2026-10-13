//! 条目图标：哪个图形、什么颜色。
//!
//! 列表视图和图标视图共用这一份。原来两边各抄了一份 `icon_for`，
//! 改一处忘一处。
//!
//! 图形取自 Lucide 全集（所以 `main.rs` 挂的是 `AllAssets` 而不是那 101 个
//! 默认图标）。颜色不写死十六进制，走主题里那几个基色——换深色主题时
//! 它们会跟着走，写死的话在深色背景上会闷成一团。

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon};
use gpui_kit::{App, Hsla, Styled as _};

use s3browser::browser_state::Entry;
use s3browser::file_kind::FileKind;

/// 实心文件夹。Lucide 全是线条图标，这个是自己加的。
///
/// 文件夹是列表里最多的东西，也是唯一"里面还有东西"的东西，给它填实就有了
/// 分量，扫一眼能从一屏文件里分出来。别的还是线条——全填实反倒又糊成一片。
const FOLDER_FILLED: &[u8] = include_bytes!("../../assets/icons/folder-filled.svg");

/// 这一类用哪个图形。文件夹走自带的那张，不在 Lucide 里。
fn glyph(kind: FileKind) -> Icon {
    let name = match kind {
        FileKind::Folder => return Icon::default().data(FOLDER_FILLED),
        FileKind::Text => IconName::FileText,
        FileKind::Code => IconName::FileCode,
        FileKind::Image => IconName::FileImage,
        FileKind::Video => IconName::FileVideoCamera,
        FileKind::Audio => IconName::FileMusic,
        FileKind::Archive => IconName::FileArchive,
        FileKind::Sheet => IconName::FileSpreadsheet,
        FileKind::Document => IconName::FileType,
        FileKind::Program => IconName::FileTerminal,
        FileKind::Other => IconName::File,
    };
    Icon::new(name)
}

/// 这一类涂什么颜色。
///
/// 只用主题里那六个基色，不用 `_light` 变体——浅色变体在白底上太淡，
/// 16 像素的线条图标本来就没几个像素，一淡就看不清了。
///
/// 六个颜色分十种类，所以有几对共用一个色：共用的两类图形差得远
/// （压缩包和文档、图片和表格），认的是图形，颜色只负责分组。
/// 文件夹给暖黄——那是所有文件管理器几十年的约定，换个颜色反而要多看一眼。
fn color_for(kind: FileKind, cx: &App) -> Hsla {
    let theme = cx.theme();
    match kind {
        FileKind::Folder => theme.yellow,
        // 读的、写的
        FileKind::Text | FileKind::Code => theme.blue,
        // 数据
        FileKind::Image | FileKind::Sheet => theme.green,
        // 媒体
        FileKind::Video | FileKind::Audio => theme.magenta,
        // 打好包的
        FileKind::Archive | FileKind::Document => theme.red,
        // 能跑的
        FileKind::Program => theme.cyan,
        // 认不出来的不给颜色，正好和认出来的拉开
        FileKind::Other => theme.muted_foreground,
    }
}

/// 一行条目要画的图标，颜色已经上好，调用方只管挑个尺寸。
pub fn for_entry(entry: &Entry, cx: &App) -> Icon {
    let kind = FileKind::of(&entry.name, entry.is_dir);
    glyph(kind).text_color(color_for(kind, cx))
}
