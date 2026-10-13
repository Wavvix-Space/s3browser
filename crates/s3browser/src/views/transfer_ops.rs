//! 把东西搬来搬去：上传、下载、复制、剪切、粘贴，以及拖放落地。
//!
//! 这些最后都归结成同一件事——造一批任务交给引擎，所以都走 `submit_batch`。

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::*;
use s3api::S3Client;
use transfer::job::{Batch, Job, JobKind};

use s3browser::browser_state::{Item, Location};
use s3browser::ops;

use crate::views::main_window::MainWindow;

/// 复制或剪切下来的东西，等着粘贴到别处。
#[derive(Clone)]
pub struct Clipboard {
    pub source: Location,
    pub items: Vec<Item>,
    /// 剪切为真，复制为假。
    pub cut: bool,
}

/// 入队时要不要在右下角报一声「开始…」。
///
/// 删除、新建文件夹、重命名都是一瞬间的事，报了只是吵；
/// 上传下载复制可能跑很久，得让人知道已经开始了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Announce {
    Start,
    Quiet,
}

/// 被跳过的文件名列个单：最多点三个名，再多就收成「等 N 个」。
fn list_names(names: &[String]) -> String {
    const MAX: usize = 3;
    match names.len() {
        0 => String::new(),
        n if n <= MAX => names.join("、"),
        n => format!("{} 等 {n} 个", names[..MAX].join("、")),
    }
}

impl MainWindow {
    /// 把一批任务交给引擎。
    ///
    /// 标签得等任务算完才写得对：传一个文件夹会展开成一堆任务，
    /// 不是界面上看到的"1 项"。返回是否真的入队了。
    pub(crate) fn submit_batch(
        &mut self,
        mut batch: Batch,
        jobs: Vec<Job>,
        label: impl FnOnce(usize) -> String,
        empty_hint: Option<&str>,
        announce: Announce,
        cx: &mut Context<Self>,
    ) -> bool {
        if jobs.is_empty() {
            if let Some(hint) = empty_hint {
                self.toast_warn(hint.to_owned(), cx);
            }
            return false;
        }
        batch.label = label(jobs.len());
        let started = format!("开始{}", batch.label);

        self.app.read(cx).engine.enqueue(batch, jobs);
        // 不抢视图——传输在后台跑，看不看是用户的事
        if announce == Announce::Start {
            self.toast_info(started, cx);
        }
        true
    }

    /// 选本地文件传到当前目录。
    pub fn upload_files(&mut self, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        // Windows 的原生对话框不能同时选文件和文件夹，这里只收文件
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("上传".into()),
        });

        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update(cx, |this, cx| this.enqueue_uploads(location, paths, cx))
                .ok();
        })
        .detach();
    }

    /// 选本地文件传上去。上传对话框和外部拖放都汇到这条。
    fn enqueue_uploads(&mut self, location: Location, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let skip = self.app.read(cx).config.transfer.skip_files.clone();
        let batch = Batch::new(JobKind::Upload, String::new());
        let (jobs, skipped) = ops::upload_jobs(&batch, &location, &paths, &skip);
        if jobs.is_empty() {
            let hint = if skipped.is_empty() {
                "这里面没有能传的文件".to_owned()
            } else {
                format!(
                    "没有能传的文件，{} 个被跳过：{}",
                    skipped.len(),
                    list_names(&skipped)
                )
            };
            self.toast_warn(hint, cx);
            return;
        }
        self.submit_batch(
            batch,
            jobs,
            |n| format!("上传 {n} 个文件"),
            None,
            Announce::Start,
            cx,
        );
        if !skipped.is_empty() {
            self.toast_warn(
                format!("跳过 {} 个文件：{}", skipped.len(), list_names(&skipped)),
                cx,
            );
        }
    }

    /// 选一个本地文件夹整个传上去，目录结构原样保留。
    pub fn upload_folder(&mut self, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("上传文件夹".into()),
        });

        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update(cx, |this, cx| this.enqueue_uploads(location, paths, cx))
                .ok();
        })
        .detach();
    }

    /// 外部文件拖进来了。`folder` 是落点那个子目录的名字，拖到空白处就是 None。
    pub fn drop_into_folder(
        &mut self,
        folder: Option<String>,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            self.toast_warn("先选一个桶再把文件拖进来", cx);
            return;
        };
        if paths.is_empty() {
            return;
        }
        let target = match &folder {
            Some(name) => location.child(name),
            None => location,
        };
        tracing::info!(
            bucket = %target.bucket,
            prefix = %target.prefix,
            count = paths.len(),
            "拖入上传"
        );
        self.enqueue_uploads(target, paths, cx);
    }

    /// 应用内把选中项拖到某个文件夹上。默认移动，按住 Ctrl 是复制。
    pub fn drop_entries_into(&mut self, folder: String, cut: bool, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let items = self.browser().read(cx).selected_items();
        // 拖到自己身上什么都不用做
        if items.is_empty() || items.iter().any(|item| item.name == folder) {
            return;
        }

        let target = location.child(&folder);
        let kind = if cut { JobKind::Move } else { JobKind::Copy };
        let batch = Batch::new(kind, String::new());
        let jobs = ops::paste_jobs(&batch, &location, &target, &items, cut);
        self.submit_batch(
            batch,
            jobs,
            |n| format!("{} {n} 项到 {folder}", kind.label()),
            Some("这些条目还不能这样拖"),
            Announce::Start,
            cx,
        );
        cx.notify();
    }

    /// 把选中的文件下载到某个目录。
    /// 下载选中的东西。文件夹会递归列举后整棵一起下。
    pub fn download_selected(&mut self, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let items = self.browser().read(cx).selected_items();
        if items.is_empty() {
            return;
        }
        // 只有文件夹时才需要客户端去列举，但先取好省得回调里再拿
        let client = self.client_for(&location.profile, cx);

        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("下载到这里".into()),
        });

        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(dirs))) = picked.await else {
                return;
            };
            let Some(dir) = dirs.into_iter().next() else {
                return;
            };

            let batch = Batch::new(JobKind::Download, String::new());
            let (folders, files): (Vec<Item>, Vec<Item>) =
                items.into_iter().partition(|item| item.is_dir);
            // 重名时按 Finder 的规矩加序号，不覆盖已有文件
            let mut jobs = ops::download_jobs(&batch, &location, &dir, &files);

            for folder in &folders {
                let Some(client) = client.clone() else { break };
                let prefix = ops::folder_key(&location, &folder.name);
                let objects = list_all(&client, &location.bucket, &prefix).await;
                // 加序号的是整个目录，不是里面每个文件——不然目录结构会被
                // 「a.jpg / a 2.jpg」这种拆得七零八落
                let root = transfer::unique_path(&dir.join(&folder.name));
                jobs.extend(ops::folder_download_jobs(
                    &batch, &location, &root, &prefix, &objects,
                ));
            }

            this.update(cx, |this, cx| {
                this.submit_batch(
                    batch,
                    jobs,
                    |n| format!("下载 {n} 个文件"),
                    Some("选中的文件夹里没有文件"),
                    Announce::Start,
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    pub fn copy_selected(&mut self, cut: bool, cx: &mut Context<Self>) {
        let Some(location) = self.browser().read(cx).location().cloned() else {
            return;
        };
        let items = self.browser().read(cx).selected_items();
        if items.is_empty() {
            return;
        }
        self.clipboard = Some(Clipboard {
            source: location,
            items,
            cut,
        });
        cx.notify();
    }

    /// 粘贴到当前目录。同一个账户同一个桶内才行。
    pub fn paste(&mut self, cx: &mut Context<Self>) {
        let (Some(clipboard), Some(target)) = (
            self.clipboard.clone(),
            self.browser().read(cx).location().cloned(),
        ) else {
            return;
        };
        let kind = if clipboard.cut {
            JobKind::Move
        } else {
            JobKind::Copy
        };
        let batch = Batch::new(kind, String::new());
        // 跨桶、文件夹、原地粘贴都会在这里被筛掉
        let jobs = ops::paste_jobs(
            &batch,
            &clipboard.source,
            &target,
            &clipboard.items,
            clipboard.cut,
        );
        let queued = self.submit_batch(
            batch,
            jobs,
            |n| format!("{} {n} 项", kind.label()),
            Some("跨桶和文件夹还不能粘贴"),
            Announce::Start,
            cx,
        );
        if queued && clipboard.cut {
            self.clipboard = None;
        }
        cx.notify();
    }
}

/// 把一个前缀下面的对象全列出来，含子目录。
///
/// 不给 delimiter，一次就把整棵子树列进来。中途某一页失败就把已经拿到的
/// 交回去——下一半总比一个都没有强，失败的那些用户再点一次就是。
async fn list_all(client: &Arc<S3Client>, bucket: &str, prefix: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let Ok(page) = client
            .list_objects_v2(bucket, prefix, None, token.as_deref(), 1000)
            .await
        else {
            break;
        };
        out.extend(page.objects.into_iter().map(|o| (o.key, o.size)));
        match page.next_token {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    out
}
