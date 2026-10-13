//! 就地编辑：下到临时文件，用本机编辑器打开，存一次传一次。
//!
//! 不走传输引擎。能编辑的都是文本，几十 KB 撑死，塞进传输队列反而让那一页
//! 全是一闪而过的小任务；直接一次 GET 一次 PUT 就够。
//!
//! 传回去的时机是"临时文件变了"，不是"编辑器退出"——记事本可以存很多次，
//! 每存一次都该传。轮询而不是监视文件系统：一秒一次 metadata 的开销可以忽略，
//! 换来不引入 notify 那一整套依赖。

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime};

use gpui_kit::*;
use s3api::Body;

use crate::views::main_window::MainWindow;
use crate::views::preview::read_capped;

/// 能编辑多大的文件。再大就不是"顺手改一下"了，而且要整个读进内存。
const MAX_EDIT_BYTES: u64 = 32 * 1024 * 1024;

/// 多久看一眼临时文件。
const POLL: Duration = Duration::from_secs(1);

/// 编辑器退出之后还等一会儿再收摊。
///
/// 用系统关联程序打开时（比如 .md 关联到别的编辑器），我们 spawn 的往往只是
/// 个中转进程，它转手就退了，真正的编辑器还开着。所以进程没了也不能立刻停，
/// 再盯一阵，这段时间里存盘照样传得回去。
const LINGER: Duration = Duration::from_secs(300);

impl MainWindow {
    /// 编辑选中的那个文件。
    pub fn edit_selected(&mut self, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let selected = self.browser().read(cx).selected_entries();
        let [entry] = selected.as_slice() else {
            self.toast_warn("一次只能编辑一个文件", cx);
            return;
        };
        if entry.is_dir {
            self.toast_warn("文件夹没法编辑", cx);
            return;
        }
        let name = entry.name.to_string();
        let size = entry.size;
        let config = self.app.read(cx).config.edit.clone();
        if !config.allows(&name) {
            self.toast_warn(format!("{name} 不在可编辑的后缀名单里，去设置里加一个"), cx);
            return;
        }
        if size > MAX_EDIT_BYTES {
            self.toast_warn("超过 32 MiB 的文件不给编辑", cx);
            return;
        }
        let key = location.key_of(&name);
        let Some(client) = self.client_for(&location.profile, cx) else {
            return;
        };

        cx.spawn(async move |this, cx| {
            // 读下来。cap 给 +1，好分辨"正好到上限"和"被截断了"
            let bytes = match read_capped(&client, &location.bucket, &key, MAX_EDIT_BYTES + 1).await
            {
                Ok(bytes) if bytes.len() as u64 <= MAX_EDIT_BYTES => bytes,
                Ok(_) => {
                    report(&this, cx, Err("文件比列表里写的还大，不编辑了".to_owned()));
                    return;
                }
                Err(e) => {
                    report(&this, cx, Err(format!("下载失败：{e}")));
                    return;
                }
            };

            let dir = scratch_dir();
            let path = dir.join(&name);
            if let Err(e) =
                std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &bytes))
            {
                report(&this, cx, Err(format!("写临时文件失败：{e}")));
                return;
            }

            let child = match spawn_editor(config.command.as_deref(), &path) {
                Ok(child) => child,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    report(&this, cx, Err(format!("打不开编辑器：{e}")));
                    return;
                }
            };
            report(&this, cx, Ok(format!("{name} 已打开，存盘就会传回去")));

            watch_and_upload(
                &this,
                cx,
                Watched {
                    child,
                    dir,
                    path,
                    name,
                    key,
                    bucket: location.bucket.clone(),
                    client,
                },
            )
            .await;
        })
        .detach();
    }
}

struct Watched {
    child: Child,
    /// 临时目录，收摊时整个删掉。
    dir: PathBuf,
    path: PathBuf,
    name: String,
    key: String,
    bucket: String,
    client: std::sync::Arc<s3api::S3Client>,
}

/// 盯着临时文件，变一次传一次，直到编辑器退出且又安静了一阵。
async fn watch_and_upload(this: &WeakEntity<MainWindow>, cx: &mut AsyncApp, mut w: Watched) {
    let mut last = stamp(&w.path);
    let mut exited_at: Option<std::time::Instant> = None;

    loop {
        cx.background_executor().timer(POLL).await;

        // 先看文件再看进程：存完立刻关掉编辑器时，这一轮还能把改动捞回来
        let now = stamp(&w.path);
        if now.is_some() && now != last {
            last = now;
            let result = match std::fs::read(&w.path) {
                Ok(bytes) => w
                    .client
                    .put_object(&w.bucket, &w.key, Body::from(bytes), None, None)
                    .await
                    .map(|_| format!("{} 已保存", w.name))
                    .map_err(|e| format!("{} 传回去失败：{e}", w.name)),
                Err(e) => Err(format!("读不到临时文件：{e}")),
            };
            report(this, cx, result);
        }

        match w.child.try_wait() {
            Ok(Some(_)) => {
                let since = exited_at.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= LINGER {
                    break;
                }
            }
            // 还活着，或者问不出来（问不出来就当还活着，宁可多盯一会儿）
            _ => exited_at = None,
        }

        // 界面没了就别再盯着了
        if this.upgrade().is_none() {
            break;
        }
    }

    let _ = std::fs::remove_dir_all(&w.dir);
}

/// 起编辑器。
///
/// 默认记事本：它一定在，也一定能开文本。没后缀的文件在 Windows 上没有关联
/// 程序，`start` 会弹"用什么打开"的框，记事本没这个问题。
fn spawn_editor(command: Option<&str>, path: &Path) -> std::io::Result<Child> {
    match command.map(str::trim).filter(|c| !c.is_empty()) {
        Some(command) => {
            let mut parts = command.split_whitespace();
            let program = parts.next().unwrap_or("notepad.exe");
            Command::new(program).args(parts).arg(path).spawn()
        }
        None => Command::new("notepad.exe").arg(path).spawn(),
    }
}

/// 每次编辑一个单独的目录，免得同名文件撞车。
fn scratch_dir() -> PathBuf {
    std::env::temp_dir()
        .join("s3browser")
        .join("edit")
        .join(format!("{:016x}", fastrand::u64(..)))
}

/// 文件的"改没改过"指纹。只看时间不够：有些编辑器保存后时间戳精度不足，
/// 大小一起看能多接住一部分。
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn report(this: &WeakEntity<MainWindow>, cx: &mut AsyncApp, result: Result<String, String>) {
    this.update(cx, |this, cx| match result {
        Ok(message) => this.toast_ok(message, cx),
        Err(message) => this.toast_err(message, cx),
    })
    .ok();
}
