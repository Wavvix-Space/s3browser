//! 检查更新、换新版。发布信息长什么样、怎么下、怎么换，都在 `s3browser::update` 里。
//!
//! 两个入口：启动时自动查一次（设置里能关），以及设置页「关于」里手按。
//! 查到有新版本就在右下角冒一条带「查看」的提示，点过去落在设置页的「关于」上，
//! 那儿有「立刻更新」和「立刻更新并重启」。

use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::Sizable as _;
use gpui_kit::{Context, Window};

use crate::views::main_window::MainWindow;
use s3browser::update::{self, Progress, UpdateManifest, UpdateStatus};

/// 一次「立刻更新」进行到哪了。设置页按它换按钮。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpdatePhase {
    /// 没在更新。
    #[default]
    Idle,
    /// 正在下载：已下载字节、总字节。
    Downloading { done: u64, total: Option<u64> },
    /// 换好了，等着重启才生效。
    Installed,
    /// 上回没成，原因留着显示。
    Failed(String),
}

impl MainWindow {
    /// 到 GitHub Releases 查一次最新版。
    ///
    /// `manual` 是设置页里手按的「检查更新」：没配仓库、或者查失败，都得当场说一句。
    /// 启动那次不吭声——开个程序被没配好的更新源弹一脸不合适。
    pub fn check_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.update_status == UpdateStatus::Checking {
            return; // 已经在查了，别摞第二个请求上去
        }
        let Some(repo) = update::update_repo() else {
            if manual {
                self.toast_warn("这个构建没配更新仓库", cx);
            } else {
                tracing::debug!("没配更新仓库，跳过检查");
            }
            return;
        };

        self.update_status = UpdateStatus::Checking;
        cx.notify();

        let handle = self.app.read(cx).runtime_handle();
        cx.spawn(async move |this, cx| {
            // 启动那次等窗口站稳再查。查得太早，提示会赶在第一帧画完之前推进去，
            // 结果什么都不显示。
            if !manual {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(1500))
                    .await;
            }
            let fetched = handle
                .spawn(async move { update::fetch_latest(&repo).await })
                .await;
            this.update(cx, |this, cx| match fetched {
                Ok(Ok(manifest)) => this.on_manifest(manifest, cx),
                Ok(Err(e)) => {
                    tracing::warn!("检查更新失败：{e}");
                    this.update_status = UpdateStatus::Failed(e.clone());
                    if manual {
                        this.toast_warn(format!("检查更新失败：{e}"), cx);
                    }
                    cx.notify();
                }
                Err(e) => {
                    tracing::warn!("检查更新的任务没跑成：{e}");
                    this.update_status = UpdateStatus::Failed(e.to_string());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// 发布到手：记下来给设置页用，比当前版本新就在右下角提示。
    fn on_manifest(&mut self, manifest: UpdateManifest, cx: &mut Context<Self>) {
        let current = env!("CARGO_PKG_VERSION");
        let newer = update::is_newer(current, &manifest.version);
        tracing::info!(current, latest = %manifest.version, newer, "拿到发布信息");

        if !newer {
            self.update_status = UpdateStatus::UpToDate {
                latest: manifest.version,
            };
            cx.notify();
            return;
        }

        let version = manifest.version.clone();
        self.update_status = UpdateStatus::Available(manifest);
        // 版本号变了，之前那次下载的进度不作数
        self.update_phase = UpdatePhase::Idle;

        // 带「查看」的提示：点过去落在设置页的「关于」上
        let main = cx.entity().downgrade();
        let note = Notification::info(format!("最新 {version}，当前 {current}"))
            .title("发现新版本")
            .action(move |_, _, _| {
                let main = main.clone();
                Button::new("update-view")
                    .label("查看")
                    .small()
                    .on_click(move |_, window, cx| {
                        main.update(cx, |this, cx| this.open_about_section(window, cx))
                            .ok();
                    })
            });
        self.toast(note, cx);
        cx.notify();
    }

    /// 提示里「查看」的去处：切到设置页，滚到「关于」。
    pub fn open_about_section(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_settings(window, cx);
        // 「关于」是设置页最后一格，滚到底就是它
        self.settings_scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 「立刻更新」/「立刻更新并重启」。
    ///
    /// 下到数据目录的 `updates/` 里，下完校验、顶替，`restart` 为真就顺手把新程序
    /// 拉起来再退出自己。
    pub fn start_update(&mut self, restart: bool, cx: &mut Context<Self>) {
        if matches!(self.update_phase, UpdatePhase::Downloading { .. }) {
            return; // 已经在下了
        }
        let UpdateStatus::Available(manifest) = self.update_status.clone() else {
            return; // 按钮只在有新版本时才显示，这里防御一下
        };
        let Some(url) = manifest.url.clone() else {
            self.toast_warn("这个发布里没有可下载的 exe，更新不了", cx);
            return;
        };
        let dest = s3browser::config::data_dir()
            .join("updates")
            .join(format!("s3browser-{}.exe", manifest.version));

        self.update_phase = UpdatePhase::Downloading {
            done: 0,
            total: manifest.size,
        };
        cx.notify();

        let handle = self.app.read(cx).runtime_handle();
        let (progress_tx, progress_rx) = async_channel::bounded::<Progress>(8);

        // 进度一条条转回界面。下载任务拿着发送端，它结束时发送端被丢掉，
        // 这个循环自然就结束了。
        cx.spawn(async move |this, cx| {
            while let Ok((done, total)) = progress_rx.recv().await {
                this.update(cx, |this, cx| {
                    // 落定之后来的迟到进度不该把按钮又变回「正在下载」
                    if matches!(this.update_phase, UpdatePhase::Downloading { .. }) {
                        this.update_phase = UpdatePhase::Downloading { done, total };
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let sha256 = manifest.sha256.clone();
            let target = dest.clone();
            let result = handle
                .spawn(async move {
                    update::download_update(&url, sha256.as_deref(), &target, &progress_tx).await
                })
                .await
                .unwrap_or_else(|e| Err(format!("下载任务没跑成：{e}")));
            this.update(cx, |this, cx| this.on_downloaded(result, &dest, restart, cx))
                .ok();
        })
        .detach();
    }

    /// 下载落定：成了就换文件，败了记下原因。
    fn on_downloaded(
        &mut self,
        result: Result<(), String>,
        downloaded: &std::path::Path,
        restart: bool,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = result {
            tracing::warn!("下载新版失败：{e}");
            self.update_phase = UpdatePhase::Failed(e.clone());
            self.toast_err(format!("下载新版本失败：{e}"), cx);
            cx.notify();
            return;
        }

        match update::install(downloaded) {
            Ok(retired) => {
                tracing::info!(?retired, restart, "新版已换上");
                self.update_phase = UpdatePhase::Installed;
                cx.notify();
                if restart {
                    self.restart(cx);
                } else {
                    self.toast_ok("已换上新版，下次启动生效", cx);
                }
            }
            Err(e) => {
                tracing::warn!("换新版失败：{e}");
                self.update_phase = UpdatePhase::Failed(e.clone());
                self.toast_err(format!("更新失败：{e}。手动换文件吧"), cx);
                cx.notify();
            }
        }
    }

    /// 把换好的新程序拉起来，然后退出自己。
    ///
    /// 拉不起来不算白干——文件已经换好了，下次打开就是新版。
    fn restart(&mut self, cx: &mut Context<Self>) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                self.toast_warn(format!("新版已换上，但定位不到程序（{e}），手动重开吧"), cx);
                return;
            }
        };
        match std::process::Command::new(&exe).spawn() {
            Ok(_) => cx.quit(),
            Err(e) => self.toast_warn(
                format!("新版已换上，但启动它失败（{e}），下次打开就是新版了"),
                cx,
            ),
        }
    }
}
