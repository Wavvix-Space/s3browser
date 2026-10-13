//! 按大小滚动的日志文件。
//!
//! `tracing-appender` 只按时间滚（每天、每小时），这个程序的日志量跟使用强度走，
//! 和日期没关系，所以自己写一个按字节数滚的。超过上限就把
//! `s3browser.log` 改名成 `.1`，原来的 `.1` 变 `.2`，依次往后推，
//! 推过保留份数的直接删掉。

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 单个日志文件的上限。
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// 除当前这份外还留几份历史。
pub const KEEP: usize = 3;

/// 写日志的文件句柄，写满就滚一份。
///
/// `tracing_subscriber` 要求 writer 是 `Send`，这里没有内部可变状态，
/// 每次写之前看一眼累计字节数就够了。
pub struct RollingFile {
    path: PathBuf,
    /// 滚动期间要先松开句柄——Windows 上占着的文件改不了名。
    file: Option<File>,
    written: u64,
    max_bytes: u64,
    keep: usize,
}

impl RollingFile {
    pub fn new(path: impl Into<PathBuf>) -> io::Result<Self> {
        Self::with_limits(path, MAX_BYTES, KEEP)
    }

    pub fn with_limits(path: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = open_append(&path)?;
        // 接着上次的长度算，重启不会把已经写满的文件再撑大一倍
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(RollingFile {
            path,
            file: Some(file),
            written,
            max_bytes,
            keep,
        })
    }

    fn roll(&mut self) -> io::Result<()> {
        // 关掉句柄再改名
        self.file = None;

        // 旧的往后推一位，最老的那份删掉
        for n in (1..=self.keep).rev() {
            let from = numbered(&self.path, n);
            if !from.exists() {
                continue;
            }
            if n == self.keep {
                let _ = std::fs::remove_file(&from);
            } else {
                let _ = std::fs::rename(&from, numbered(&self.path, n + 1));
            }
        }

        if self.keep > 0 {
            std::fs::rename(&self.path, numbered(&self.path, 1))?;
        } else {
            std::fs::remove_file(&self.path)?;
        }

        self.file = Some(open_append(&self.path)?);
        self.written = 0;
        Ok(())
    }

    fn handle(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            // 上一次滚动没成功，重新开一个继续写
            self.file = Some(open_append(&self.path)?);
        }
        Ok(self.file.as_mut().expect("刚补上"))
    }
}

impl Write for RollingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.max_bytes && self.written > 0 {
            // 滚不动就接着往原来的文件里写，记日志不该把程序拖垮
            let _ = self.roll();
        }
        let n = self.handle()?.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// `s3browser.log` 加上序号变成 `s3browser.log.1`。
fn numbered(path: &Path, n: usize) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{n}"));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("s3b-log-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rolls_when_the_file_fills_up() {
        let dir = tmp();
        let path = dir.join("app.log");
        let mut log = RollingFile::with_limits(&path, 100, 3).unwrap();

        for _ in 0..5 {
            log.write_all(&[b'x'; 60]).unwrap();
        }
        log.flush().unwrap();

        // 60 字节写五次，每写满 100 就滚一次
        assert!(path.exists(), "当前这份要在");
        assert!(numbered(&path, 1).exists(), "滚出来的 .1 要在");
        drop(log);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keeps_only_the_configured_number_of_files() {
        let dir = tmp();
        let path = dir.join("app.log");
        let mut log = RollingFile::with_limits(&path, 50, 2).unwrap();

        for _ in 0..20 {
            log.write_all(&[b'y'; 40]).unwrap();
        }
        log.flush().unwrap();
        drop(log);

        assert!(path.exists());
        assert!(numbered(&path, 1).exists());
        assert!(numbered(&path, 2).exists());
        assert!(!numbered(&path, 3).exists(), "超过保留份数的要删掉");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 重启之后接着上次的长度算，不然一个写满的文件会被撑到两倍大。
    #[test]
    fn picks_up_the_existing_length_on_open() {
        let dir = tmp();
        let path = dir.join("app.log");
        std::fs::write(&path, vec![b'z'; 90]).unwrap();

        let mut log = RollingFile::with_limits(&path, 100, 2).unwrap();
        log.write_all(&[b'w'; 40]).unwrap();
        log.flush().unwrap();
        drop(log);

        assert!(numbered(&path, 1).exists(), "开着就该发现已经写满了");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 40);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn numbering_appends_after_the_extension() {
        assert_eq!(
            numbered(Path::new("/a/s3browser.log"), 2),
            PathBuf::from("/a/s3browser.log.2")
        );
    }
}
