//! 按扩展名把文件归个大类。
//!
//! 界面拿这个类挑图标和颜色。分类只按扩展名，不看内容——列目录时
//! 服务端只给了 key 和大小，为了画个图标去 HEAD 十万个对象不值当。
//!
//! 粒度是「一眼能分出来」那种：图片、视频、音频、压缩包、代码、表格、
//! 文档、可执行。再细下去颜色就不够用了，用户也分不清。

/// 取文件名的扩展名，没有就是 `None`。
///
/// 开头那个点不算分隔符：`.gitignore` 是个没有扩展名的隐藏文件，
/// 不是「扩展名为 gitignore」的文件。末尾是点的也当没有。
pub fn extension_of(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    if dot == 0 || dot + 1 >= name.len() {
        return None;
    }
    Some(&name[dot + 1..])
}

/// 文件的大类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Folder,
    /// 纯文本、配置、日志。
    Text,
    /// 源码和脚本。
    Code,
    Image,
    Video,
    Audio,
    Archive,
    /// 表格和数据库。
    Sheet,
    /// 文档：pdf、doc、ppt 这些要专门程序打开的。
    Document,
    /// 可执行文件和安装包。
    Program,
    /// 认不出来的。
    Other,
}

impl FileKind {
    /// 按名字和「是不是目录」判个大类。
    ///
    /// `name` 是显示名，不是整条 key——扩展名从最后一个点后面取。
    pub fn of(name: &str, is_dir: bool) -> FileKind {
        if is_dir {
            return FileKind::Folder;
        }
        let Some(ext) = extension_of(name) else {
            return FileKind::Other;
        };
        // 扩展名不区分大小写，S3 上 .JPG 和 .jpg 都常见
        match ext.to_ascii_lowercase().as_str() {
            "txt" | "md" | "markdown" | "log" | "rst" | "text" | "nfo" => FileKind::Text,
            "json" | "xml" | "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg" | "properties"
            | "env" => FileKind::Text,

            "rs" | "go" | "py" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "java" | "kt"
            | "c" | "h" | "cpp" | "cc" | "hpp" | "cs" | "rb" | "php" | "swift" | "lua" | "pl"
            | "scala" | "dart" | "vue" | "html" | "htm" | "css" | "scss" | "less" | "sql" => {
                FileKind::Code
            }
            "sh" | "bash" | "zsh" | "fish" | "ps1" | "psm1" | "bat" | "cmd" => FileKind::Code,

            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff"
            | "heic" | "heif" | "avif" | "raw" | "psd" => FileKind::Image,

            // `.ts` 不在这儿：它既是 TypeScript 又是 MPEG 传输流，
            // 这个仓库里前者常见得多，归代码
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg"
            | "m2ts" | "rmvb" => FileKind::Video,

            "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" | "aiff" => {
                FileKind::Audio
            }

            "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "tgz" | "lz4" | "cab"
            | "iso" => FileKind::Archive,

            "csv" | "tsv" | "xls" | "xlsx" | "xlsm" | "ods" | "db" | "sqlite" | "sqlite3"
            | "parquet" => FileKind::Sheet,

            "pdf" | "doc" | "docx" | "odt" | "ppt" | "pptx" | "odp" | "epub" | "mobi" | "rtf" => {
                FileKind::Document
            }

            "exe" | "msi" | "msix" | "appx" | "dll" | "so" | "dylib" | "deb" | "rpm" | "apk"
            | "dmg" | "pkg" | "jar" | "wasm" => FileKind::Program,

            _ => FileKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_is_a_directory_whatever_it_is_called() {
        // 目录名里带点很常见，`v1.8.9/` 不该被当成扩展名 9 的文件
        assert_eq!(FileKind::of("v1.8.9", true), FileKind::Folder);
        assert_eq!(FileKind::of("photos", true), FileKind::Folder);
    }

    #[test]
    fn extensions_are_case_insensitive() {
        assert_eq!(FileKind::of("IMG_0001.JPG", false), FileKind::Image);
        assert_eq!(FileKind::of("IMG_0001.jpg", false), FileKind::Image);
    }

    /// `.gitignore` 整个名字就是个「扩展名」，点在第 0 位。
    /// 按扩展名切会切出 "gitignore"，认不出来，归 Other——不该 panic，
    /// 也不该把它当成什么奇怪的类型。
    #[test]
    fn a_dotfile_has_no_extension_to_speak_of() {
        assert_eq!(FileKind::of(".gitignore", false), FileKind::Other);
        assert_eq!(FileKind::of("Makefile", false), FileKind::Other);
        // 点在末尾，后面什么都没有
        assert_eq!(FileKind::of("weird.", false), FileKind::Other);
    }

    /// 多重扩展名只认最后一段。`.tar.gz` 归压缩包是因为 gz 在表里。
    #[test]
    fn only_the_last_extension_counts() {
        assert_eq!(FileKind::of("backup.tar.gz", false), FileKind::Archive);
        assert_eq!(FileKind::of("app.min.js", false), FileKind::Code);
    }

    #[test]
    fn the_big_categories_land_where_they_should() {
        assert_eq!(FileKind::of("a.mp4", false), FileKind::Video);
        assert_eq!(FileKind::of("a.flac", false), FileKind::Audio);
        assert_eq!(FileKind::of("a.xlsx", false), FileKind::Sheet);
        assert_eq!(FileKind::of("a.pdf", false), FileKind::Document);
        assert_eq!(FileKind::of("a.exe", false), FileKind::Program);
        assert_eq!(FileKind::of("a.toml", false), FileKind::Text);
        assert_eq!(FileKind::of("a.rs", false), FileKind::Code);
        assert_eq!(FileKind::of("a.bin", false), FileKind::Other);
    }
}
