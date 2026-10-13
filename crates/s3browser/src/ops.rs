//! 由界面动作生成传输任务。
//!
//! 这里是纯函数：给定当前位置和选中项，算出要交给引擎的任务列表。
//! 视图只负责收集输入和把结果丢进引擎，所以这部分能单独测。

use std::path::{Path, PathBuf};

use transfer::job::{Batch, Job, JobKind};

use crate::browser_state::{Item, Location};

/// 文件夹在 S3 里就是一个以 `/` 结尾的空对象。
pub fn folder_key(location: &Location, name: &str) -> String {
    format!("{}{}/", location.prefix, name)
}

/// 选中项对应的 key。文件夹带尾斜杠，引擎看到它会去递归列举。
pub fn item_key(location: &Location, name: &str, is_dir: bool) -> String {
    if is_dir {
        folder_key(location, name)
    } else {
        location.key_of(name)
    }
}

pub fn new_folder_job(batch: &Batch, location: &Location, name: &str) -> Job {
    Job::new(
        &batch.id,
        JobKind::Upload,
        &location.profile,
        &location.bucket,
        &folder_key(location, name),
    )
}

/// 重命名就是同目录内的移动。名字没变就返回 None。
///
/// 文件夹的 `old_key` 带尾斜杠，目标也带——引擎看见尾斜杠就知道要递归列举，
/// 把底下每个对象都搬到新前缀下面。
pub fn rename_job(
    batch: &Batch,
    location: &Location,
    old_key: &str,
    new_name: &str,
    size: u64,
) -> Option<Job> {
    let is_dir = old_key.ends_with('/');
    let target = if is_dir {
        folder_key(location, new_name)
    } else {
        location.key_of(new_name)
    };
    if target == old_key {
        return None;
    }
    Some(
        Job::new(
            &batch.id,
            JobKind::Move,
            &location.profile,
            &location.bucket,
            &target,
        )
        .with_source(old_key, size),
    )
}

pub fn delete_jobs(batch: &Batch, location: &Location, items: &[Item]) -> Vec<Job> {
    items
        .iter()
        .map(|item| {
            Job::new(
                &batch.id,
                JobKind::Delete,
                &location.profile,
                &location.bucket,
                &item_key(location, &item.name, item.is_dir),
            )
        })
        .collect()
}

/// 粘贴：同桶内的复制或移动。文件夹和原地粘贴会被跳过。
pub fn paste_jobs(
    batch: &Batch,
    from: &Location,
    to: &Location,
    items: &[Item],
    cut: bool,
) -> Vec<Job> {
    if from.profile != to.profile || from.bucket != to.bucket {
        return Vec::new();
    }
    let kind = if cut { JobKind::Move } else { JobKind::Copy };
    items
        .iter()
        .filter(|item| !item.is_dir)
        .filter_map(|item| {
            let src = from.key_of(&item.name);
            let dst = to.key_of(&item.name);
            if src == dst {
                return None;
            }
            Some(
                Job::new(&batch.id, kind, &to.profile, &to.bucket, &dst)
                    .with_source(&src, item.size),
            )
        })
        .collect()
}

/// 目录展开的深度上限。正常目录远不到这个数，纯粹是防目录符号链接绕成环。
const MAX_UPLOAD_DEPTH: usize = 64;

/// 上传：跳过 `skip` 里列的文件名，读不到的也跳过。
///
/// 传进来的目录会递归展开，key 保留相对目录结构；空目录不产生任何对象。
/// 返回任务和被跳过的文件名——静默吞掉的话用户会以为传上了。
pub fn upload_jobs(
    batch: &Batch,
    location: &Location,
    paths: &[PathBuf],
    skip: &[String],
) -> (Vec<Job>, Vec<String>) {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    for path in paths {
        collect_uploads(path, "", skip, 0, &mut items, &mut skipped);
    }

    let jobs = items
        .into_iter()
        .map(|item| {
            Job::new(
                &batch.id,
                JobKind::Upload,
                &location.profile,
                &location.bucket,
                &location.key_of(&item.rel),
            )
            .with_local(item.path, item.size, item.mtime)
        })
        .collect();
    (jobs, skipped)
}

/// 一个待上传的本地文件。`rel` 是相对所选路径的 key 后缀，用 `/` 分隔。
struct UploadItem {
    path: PathBuf,
    rel: String,
    size: u64,
    mtime: i64,
}

fn collect_uploads(
    path: &Path,
    prefix: &str,
    skip: &[String],
    depth: usize,
    out: &mut Vec<UploadItem>,
    skipped: &mut Vec<String>,
) {
    if depth > MAX_UPLOAD_DEPTH {
        return;
    }
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    if skip.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
        skipped.push(name);
        return;
    }
    let rel = format!("{prefix}{name}");

    let Ok(meta) = std::fs::metadata(path) else {
        skipped.push(name);
        return;
    };
    if meta.is_dir() {
        let Ok(dir) = std::fs::read_dir(path) else {
            return;
        };
        // 排一下序，任务面板里的顺序才稳定
        let mut children: Vec<PathBuf> = dir.filter_map(|e| e.ok().map(|e| e.path())).collect();
        children.sort();
        for child in children {
            collect_uploads(&child, &format!("{rel}/"), skip, depth + 1, out, skipped);
        }
        return;
    }

    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    out.push(UploadItem {
        path: path.to_owned(),
        rel,
        size: meta.len(),
        mtime,
    });
}

/// 下载：目标重名时按 Finder 的规矩加序号。
///
/// 文件夹在这里被跳过——整个目录下载还没做，选中项里混了文件夹也不该报错。
pub fn download_jobs(batch: &Batch, location: &Location, dir: &Path, items: &[Item]) -> Vec<Job> {
    items
        .iter()
        .filter(|item| !item.is_dir)
        .map(|item| {
            let target = transfer::unique_path(&dir.join(&item.name));
            Job::new(
                &batch.id,
                JobKind::Download,
                &location.profile,
                &location.bucket,
                &location.key_of(&item.name),
            )
            .with_local(target, item.size, 0)
        })
        .collect()
}

/// Windows 上文件名里不能有的字符。S3 的 key 里这些全是合法的。
const ILLEGAL: &[char] = &['<', '>', ':', '"', '|', '?', '*', '\\'];

/// Windows 保留的设备名，当文件名会失败，加个下划线躲开。
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 把远端 key 里的一截相对路径变成本地能用的相对路径。
///
/// key 是服务端给的，什么都可能有：`../` 能爬出下载目录，`:` 和 `?` 在
/// Windows 上根本建不出文件，末尾的点和空格会被悄悄吃掉导致对不上。
/// 一个都不能直接拼。
///
/// 整条路径没剩下任何一段（比如 key 就是 `../..`）时返回 `None`，调用方跳过它。
pub fn safe_relative(rest: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in rest.split('/') {
        let mut name: String = part
            .chars()
            .map(|c| {
                if ILLEGAL.contains(&c) || (c as u32) < 0x20 {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        // 末尾的点和空格 Windows 会自己删掉，留着会出现"建好了却找不到"
        let trimmed = name.trim_end_matches([' ', '.']);
        name.truncate(trimmed.len());
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        // 保留名连带扩展名一起算，`con.txt` 也不行
        let stem = name.split('.').next().unwrap_or(&name).to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            name.insert(0, '_');
        }
        out.push(name);
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    Some(out)
}

/// 整个文件夹下载：远端列出来的每个对象一个任务，落到 `root` 下面。
///
/// `prefix` 是这个文件夹在桶里的完整前缀（带尾斜杠），key 去掉它之后那截
/// 原样接到 `root` 后面，子目录层级就这么保住。
///
/// 和上传文件夹一样，在这儿就把目录摊成一个个文件任务，不像复制/移动那样
/// 做成一个任务——下载是真的在搬字节，摊开才有每个文件各自的进度和续传。
///
/// 以 `/` 结尾的是目录占位对象，跳过：它不是文件，建出来会是个空文件。
pub fn folder_download_jobs(
    batch: &Batch,
    location: &Location,
    root: &Path,
    prefix: &str,
    objects: &[(String, u64)],
) -> Vec<Job> {
    objects
        .iter()
        .filter(|(key, _)| !key.ends_with('/'))
        .filter_map(|(key, size)| {
            let rest = key.strip_prefix(prefix)?;
            let target = root.join(safe_relative(rest)?);
            Some(
                Job::new(
                    &batch.id,
                    JobKind::Download,
                    &location.profile,
                    &location.bucket,
                    key,
                )
                .with_local(target, *size, 0),
            )
        })
        .collect()
}

/// 对象名的基本校验。S3 的 key 几乎什么都能放，但这几样会把路径搞乱。
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("名称不能为空".to_owned());
    }
    if name.contains('/') {
        return Err("名称里不能有斜杠".to_owned());
    }
    if name == "." || name == ".." {
        return Err("这个名称不能用".to_owned());
    }
    if name.len() > 255 {
        return Err("名称太长了".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(prefix: &str) -> Location {
        let mut location = Location::root("p1", "bucket");
        location.prefix = prefix.into();
        location
    }

    #[test]
    fn rejects_names_that_break_paths() {
        assert!(validate_name("报告.pdf").is_ok());
        assert!(validate_name("a b c").is_ok());

        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name(&"x".repeat(256)).is_err());
    }

    /// key 是服务端给的，`../` 能一路爬出下载目录。这是最要紧的一条。
    #[test]
    fn a_key_cannot_climb_out_of_the_download_folder() {
        assert_eq!(
            safe_relative("../../evil.exe"),
            Some(PathBuf::from("evil.exe"))
        );
        assert_eq!(safe_relative("a/../../b"), Some(PathBuf::from("a/b")));
        // 整条路径什么都不剩，调用方得跳过它
        assert_eq!(safe_relative("../.."), None);
        assert_eq!(safe_relative(""), None);
    }

    #[test]
    fn windows_illegal_characters_become_underscores() {
        assert_eq!(safe_relative("a:b?c.txt"), Some(PathBuf::from("a_b_c.txt")));
        // 反斜杠不能留，不然它在 Windows 上又变成一级目录
        assert_eq!(safe_relative(r"a\b.txt"), Some(PathBuf::from("a_b.txt")));
    }

    /// Windows 会把结尾的点和空格悄悄删掉，留着就会"建好了却找不到"。
    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        assert_eq!(safe_relative("name. ."), Some(PathBuf::from("name")));
        assert_eq!(
            safe_relative("dir /f.txt"),
            Some(PathBuf::from("dir/f.txt"))
        );
    }

    /// CON、NUL 这些当文件名会直接失败，连带扩展名也算。
    #[test]
    fn reserved_device_names_get_a_prefix() {
        assert_eq!(safe_relative("CON"), Some(PathBuf::from("_CON")));
        assert_eq!(safe_relative("con.txt"), Some(PathBuf::from("_con.txt")));
        assert_eq!(
            safe_relative("console.txt"),
            Some(PathBuf::from("console.txt"))
        );
    }

    #[test]
    fn folder_download_keeps_the_subdirectory_layout() {
        let batch = Batch::new(JobKind::Download, "x");
        let root = PathBuf::from("D:/out/photos");
        let objects = vec![
            ("p/photos/".to_owned(), 0),
            ("p/photos/a.jpg".to_owned(), 10),
            ("p/photos/2026/b.jpg".to_owned(), 20),
        ];
        let jobs = folder_download_jobs(&batch, &loc("p/"), &root, "p/photos/", &objects);

        // 目录占位对象不算文件，跳过
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].local_path, Some(root.join("a.jpg")));
        assert_eq!(jobs[1].local_path, Some(root.join("2026").join("b.jpg")));
        assert_eq!(jobs[1].key, "p/photos/2026/b.jpg");
    }

    #[test]
    fn folder_keys_end_with_slash() {
        assert_eq!(folder_key(&loc(""), "新建"), "新建/");
        assert_eq!(folder_key(&loc("a/b/"), "新建"), "a/b/新建/");
        assert_eq!(item_key(&loc("a/"), "x.txt", false), "a/x.txt");
        assert_eq!(item_key(&loc("a/"), "sub", true), "a/sub/");
    }

    #[test]
    fn new_folder_job_targets_the_current_directory() {
        let batch = Batch::new(JobKind::Upload, "x");
        let job = new_folder_job(&batch, &loc("docs/"), "2026");
        assert_eq!(job.key, "docs/2026/");
        assert_eq!(job.kind, JobKind::Upload);
        assert_eq!(job.batch_id, batch.id);
    }

    #[test]
    fn rename_moves_within_the_same_directory() {
        let batch = Batch::new(JobKind::Move, "x");
        let job = rename_job(&batch, &loc("docs/"), "docs/old.txt", "new.txt", 42).unwrap();
        assert_eq!(job.kind, JobKind::Move);
        assert_eq!(job.src_key, "docs/old.txt");
        assert_eq!(job.key, "docs/new.txt");
        assert_eq!(job.size, 42);
    }

    #[test]
    fn rename_to_the_same_name_does_nothing() {
        let batch = Batch::new(JobKind::Move, "x");
        assert!(rename_job(&batch, &loc("docs/"), "docs/a.txt", "a.txt", 1).is_none());
    }

    /// 文件夹改名：两头都带尾斜杠，引擎据此去递归列举。
    #[test]
    fn renaming_a_folder_keeps_the_trailing_slashes() {
        let batch = Batch::new(JobKind::Move, "x");
        let job = rename_job(&batch, &loc("docs/"), "docs/~/", "data", 0).unwrap();

        assert_eq!(job.src_key, "docs/~/");
        assert_eq!(job.key, "docs/data/");
        assert_eq!(job.kind, JobKind::Move);
    }

    #[test]
    fn renaming_a_folder_to_the_same_name_does_nothing() {
        let batch = Batch::new(JobKind::Move, "x");
        assert!(rename_job(&batch, &loc("docs/"), "docs/data/", "data", 0).is_none());
    }

    #[test]
    fn delete_marks_folders_with_a_trailing_slash() {
        let batch = Batch::new(JobKind::Delete, "x");
        let jobs = delete_jobs(
            &batch,
            &loc("docs/"),
            &[Item::new("a.txt", false, 1), Item::new("sub", true, 0)],
        );
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].key, "docs/a.txt");
        // 尾斜杠是给引擎的信号：去递归列举
        assert_eq!(jobs[1].key, "docs/sub/");
        assert!(jobs.iter().all(|j| j.kind == JobKind::Delete));
    }

    #[test]
    fn paste_copies_between_directories() {
        let batch = Batch::new(JobKind::Copy, "x");
        let items = vec![Item::new("a.txt", false, 10)];
        let jobs = paste_jobs(&batch, &loc("src/"), &loc("dst/"), &items, false);

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, JobKind::Copy);
        assert_eq!(jobs[0].src_key, "src/a.txt");
        assert_eq!(jobs[0].key, "dst/a.txt");
    }

    #[test]
    fn cut_produces_move_jobs() {
        let batch = Batch::new(JobKind::Move, "x");
        let items = vec![Item::new("a.txt", false, 10)];
        let jobs = paste_jobs(&batch, &loc("src/"), &loc("dst/"), &items, true);
        assert_eq!(jobs[0].kind, JobKind::Move);
    }

    #[test]
    fn paste_skips_folders_and_same_place() {
        let batch = Batch::new(JobKind::Copy, "x");
        let items = vec![Item::new("a.txt", false, 10), Item::new("sub", true, 0)];

        // 粘回原处：只有文件夹被跳过还不够，同名同位置的也要跳
        let same = paste_jobs(&batch, &loc("src/"), &loc("src/"), &items, false);
        assert!(same.is_empty(), "{same:?}");

        let across = paste_jobs(&batch, &loc("src/"), &loc("dst/"), &items, false);
        assert_eq!(across.len(), 1, "文件夹还不支持，应该只剩那个文件");
    }

    #[test]
    fn paste_across_buckets_is_refused() {
        let batch = Batch::new(JobKind::Copy, "x");
        let mut other = loc("dst/");
        other.bucket = "another".into();
        let items = vec![Item::new("a.txt", false, 10)];

        assert!(paste_jobs(&batch, &loc("src/"), &other, &items, false).is_empty());
    }

    #[test]
    fn upload_skips_listed_names_and_missing_files() {
        let dir = std::env::temp_dir().join(format!("s3b-ops-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("report.pdf");
        let junk = dir.join("Thumbs.db");
        std::fs::write(&good, b"hello").unwrap();
        std::fs::write(&junk, b"junk").unwrap();
        let missing = dir.join("gone.txt");

        let batch = Batch::new(JobKind::Upload, "x");
        let (jobs, skipped) = upload_jobs(
            &batch,
            &loc("docs/"),
            &[good.clone(), junk.clone(), missing],
            &["thumbs.db".to_owned()],
        );

        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].key, "docs/report.pdf");
        assert_eq!(jobs[0].size, 5);
        assert_eq!(jobs[0].local_path.as_deref(), Some(good.as_path()));
        // 名单命中的和读不到的都要点名报出来
        let mut skipped = skipped;
        skipped.sort();
        assert_eq!(skipped, ["Thumbs.db", "gone.txt"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 选中一个文件夹上传时，目录结构要原样搬到 key 上。
    #[test]
    fn upload_expands_folders_into_relative_keys() {
        let dir = std::env::temp_dir().join(format!("s3b-ops-{}", fastrand::u32(..)));
        let nested = dir.join("photos").join("2026");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("photos").join("cover.jpg"), b"a").unwrap();
        std::fs::write(nested.join("trip.jpg"), b"bb").unwrap();
        std::fs::write(nested.join("Thumbs.db"), b"junk").unwrap();
        // 空目录不该产生任何对象
        std::fs::create_dir_all(dir.join("photos").join("empty")).unwrap();

        let batch = Batch::new(JobKind::Upload, "x");
        let (jobs, skipped) = upload_jobs(
            &batch,
            &loc("docs/"),
            &[dir.join("photos")],
            &["thumbs.db".to_owned()],
        );

        let mut keys: Vec<&str> = jobs.iter().map(|j| j.key.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["docs/photos/2026/trip.jpg", "docs/photos/cover.jpg"]);
        assert_eq!(skipped, ["Thumbs.db"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_avoids_overwriting_existing_files() {
        let dir = std::env::temp_dir().join(format!("s3b-ops-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"existing").unwrap();

        let batch = Batch::new(JobKind::Download, "x");
        let jobs = download_jobs(
            &batch,
            &loc("docs/"),
            &dir,
            &[Item::new("a.txt", false, 10), Item::new("b.txt", false, 20)],
        );

        assert_eq!(jobs[0].key, "docs/a.txt");
        assert_eq!(
            jobs[0].local_path.as_deref(),
            Some(dir.join("a 2.txt").as_path())
        );
        assert_eq!(
            jobs[1].local_path.as_deref(),
            Some(dir.join("b.txt").as_path())
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 整个目录下载还没做。选中项里混了文件夹时跳过它，别造一个会失败的任务。
    #[test]
    fn download_skips_folders() {
        let dir = std::env::temp_dir().join(format!("s3b-ops-{}", fastrand::u32(..)));
        let batch = Batch::new(JobKind::Download, "x");
        let jobs = download_jobs(
            &batch,
            &loc("docs/"),
            &dir,
            &[Item::new("sub", true, 0), Item::new("a.txt", false, 10)],
        );

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].key, "docs/a.txt");
    }
}
