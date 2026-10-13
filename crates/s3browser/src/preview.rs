//! 快速预览能显示什么，由扩展名和大小决定。
//!
//! 这里只做判定，不碰 GPUI，也不下载。界面拿到 [`PreviewKind`] 再决定要不要取内容。
//! 图片格式单独定义一个枚举，免得这个 crate 为了一个类型去依赖界面库。

/// 图片预览的大小上限。再大就只显示元数据，不值得为看一眼把内存撑起来。
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// 文本预览的大小上限。
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;

/// 能直接画出来的图片格式。和 GPUI 的 `ImageFormat` 一一对应，在界面层转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    Svg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewKind {
    Image(ImageKind),
    Text,
    /// 类型认得，但超过了上限。附上给用户看的说法。
    TooLarge(&'static str),
    /// 不打算渲染内容的类型，只显示图标和元数据。
    Unsupported,
}

impl PreviewKind {
    /// 要不要下载内容。不需要的话浮层直接显示元数据。
    pub fn needs_body(&self) -> bool {
        matches!(self, PreviewKind::Image(_) | PreviewKind::Text)
    }
}

/// 按名字和大小判断这个对象怎么预览。文件夹不预览。
pub fn classify(name: &str, size: u64, is_dir: bool) -> PreviewKind {
    if is_dir {
        return PreviewKind::Unsupported;
    }
    let ext = extension_of(name);

    if let Some(kind) = image_kind(&ext) {
        return if size > MAX_IMAGE_BYTES {
            PreviewKind::TooLarge("图片超过 20 MB，不预览")
        } else {
            PreviewKind::Image(kind)
        };
    }
    if is_text(&ext) {
        return if size > MAX_TEXT_BYTES {
            PreviewKind::TooLarge("文本超过 1 MB，不预览")
        } else {
            PreviewKind::Text
        };
    }
    PreviewKind::Unsupported
}

fn extension_of(name: &str) -> String {
    match name.rfind('.') {
        Some(ix) if ix + 1 < name.len() => name[ix + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn image_kind(ext: &str) -> Option<ImageKind> {
    Some(match ext {
        "png" => ImageKind::Png,
        "jpg" | "jpeg" => ImageKind::Jpeg,
        "gif" => ImageKind::Gif,
        "webp" => ImageKind::Webp,
        "bmp" => ImageKind::Bmp,
        "svg" => ImageKind::Svg,
        _ => return None,
    })
}

/// 常见的文本和源码扩展名。不认得的一律不当文本，免得把二进制画成乱码。
fn is_text(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "json"
            | "log"
            | "csv"
            | "xml"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "conf"
            | "cfg"
            | "env"
            | "sql"
            | "sh"
            | "bat"
            | "ps1"
            | "rs"
            | "go"
            | "py"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "java"
            | "kt"
            | "rb"
            | "php"
            | "html"
            | "css"
            | "scss"
    )
}

/// 浮层里最多画多少行。下载上限是 1 MB，但真把一百万字符交给排版会卡住，
/// 看一眼用不着那么多。
pub const MAX_PREVIEW_LINES: usize = 400;
/// 同时也限制总字符数，防一行特别长的 JSON。
pub const MAX_PREVIEW_CHARS: usize = 40_000;

/// 截到能画得动的长度。第二个返回值表示截过。
pub fn clip_text(text: &str) -> (String, bool) {
    let mut out = String::new();
    let mut lines = 0;
    let mut clipped = false;

    for line in text.split_inclusive('\n') {
        if lines >= MAX_PREVIEW_LINES || out.chars().count() >= MAX_PREVIEW_CHARS {
            clipped = true;
            break;
        }
        out.push_str(line);
        lines += 1;
    }
    // 单行也可能超长，按字符再切一刀
    if out.chars().count() > MAX_PREVIEW_CHARS {
        out = out.chars().take(MAX_PREVIEW_CHARS).collect();
        clipped = true;
    }
    (out, clipped)
}

/// 文本内容可能不是 UTF-8。能解就解，解不出来按 latin-1 兜底，不报错。
pub fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.iter().map(|b| *b as char).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_recognized_by_extension() {
        assert_eq!(
            classify("photo.PNG", 1024, false),
            PreviewKind::Image(ImageKind::Png)
        );
        assert_eq!(
            classify("a/b/c.jpeg", 1024, false),
            PreviewKind::Image(ImageKind::Jpeg)
        );
        assert_eq!(
            classify("icon.svg", 10, false),
            PreviewKind::Image(ImageKind::Svg)
        );
    }

    #[test]
    fn text_covers_source_files_too() {
        assert_eq!(classify("main.rs", 100, false), PreviewKind::Text);
        assert_eq!(classify("notes.md", 100, false), PreviewKind::Text);
        assert_eq!(classify("data.json", 100, false), PreviewKind::Text);
    }

    #[test]
    fn oversized_files_are_not_fetched() {
        assert!(matches!(
            classify("big.png", MAX_IMAGE_BYTES + 1, false),
            PreviewKind::TooLarge(_)
        ));
        assert!(matches!(
            classify("big.log", MAX_TEXT_BYTES + 1, false),
            PreviewKind::TooLarge(_)
        ));
        // 正好卡在上限上还是要预览的
        assert_eq!(
            classify("big.log", MAX_TEXT_BYTES, false),
            PreviewKind::Text
        );
    }

    #[test]
    fn unknown_and_directories_show_metadata_only() {
        assert_eq!(classify("archive.zip", 10, false), PreviewKind::Unsupported);
        assert_eq!(classify("noext", 10, false), PreviewKind::Unsupported);
        // 点结尾没有扩展名
        assert_eq!(classify("trailing.", 10, false), PreviewKind::Unsupported);
        assert_eq!(classify("photo.png", 10, true), PreviewKind::Unsupported);
    }

    #[test]
    fn only_renderable_kinds_need_a_download() {
        assert!(classify("a.png", 1, false).needs_body());
        assert!(classify("a.txt", 1, false).needs_body());
        assert!(!classify("a.zip", 1, false).needs_body());
        assert!(!classify("a.png", MAX_IMAGE_BYTES + 1, false).needs_body());
    }

    #[test]
    fn clip_keeps_short_text_intact() {
        let (out, clipped) = clip_text("一行\n两行\n");
        assert_eq!(out, "一行\n两行\n");
        assert!(!clipped);
    }

    #[test]
    fn clip_stops_at_the_line_limit() {
        let text = "x\n".repeat(MAX_PREVIEW_LINES + 50);
        let (out, clipped) = clip_text(&text);
        assert!(clipped);
        assert_eq!(out.lines().count(), MAX_PREVIEW_LINES);
    }

    /// 一整个文件挤在一行里也不能把全部字符丢给排版。
    #[test]
    fn clip_stops_at_the_char_limit_on_one_long_line() {
        let text = "a".repeat(MAX_PREVIEW_CHARS * 2);
        let (out, clipped) = clip_text(&text);
        assert!(clipped);
        assert_eq!(out.chars().count(), MAX_PREVIEW_CHARS);
    }

    #[test]
    fn invalid_utf8_still_renders() {
        assert_eq!(decode_text("你好".as_bytes()), "你好");
        // 单独一个 0xff 不是合法 UTF-8，不能 panic
        assert_eq!(decode_text(&[0xff, 0x41]), "ÿA");
    }
}
