//! 设置那一格。
//!
//! 改完立刻生效，没有「保存」按钮：每个框各自订阅自己的变化，写进
//! `config.json` 再把新参数送给引擎。正在跑的任务不受影响——它们按老的
//! 分片大小切好了，中途换算法就续不上了。

use std::path::{Path, PathBuf};

use gpui_kit::component::input::{Input, InputEvent, InputState, NumberInput};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{button::*, *};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use s3browser::browser_state::{ViewMode, format_size};
use gpui_kit::component::tab::{Tab as TabHead, TabBar};

use s3browser::config::{
    EditConfig, MAX_CONNECTIONS, MAX_PARTS_PER_JOB, PART_SIZE_MIB, THRESHOLD_MIB, Theme as ThemeChoice,
    TransferConfig, config_path, data_dir,
};

use crate::views::main_window::MainWindow;
use crate::views::updates::UpdatePhase;

/// 设置格里那几个输入框的状态。开关和单选不用存，直接照配置画。
pub struct SettingsForm {
    part_size: Entity<InputState>,
    threshold: Entity<InputState>,
    connections: Entity<InputState>,
    parts_per_job: Entity<InputState>,
    skip_files: Entity<InputState>,
    edit_extensions: Entity<InputState>,
}

impl SettingsForm {
    pub fn new(
        transfer: &TransferConfig,
        edit: &EditConfig,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Self {
        let part_size = number(
            transfer.part_size_mib,
            *PART_SIZE_MIB.start(),
            *PART_SIZE_MIB.end(),
            window,
            cx,
        );
        let threshold = number(
            transfer.multipart_threshold_mib,
            *THRESHOLD_MIB.start(),
            *THRESHOLD_MIB.end(),
            window,
            cx,
        );
        let connections = number(
            transfer.max_connections as u64,
            *MAX_CONNECTIONS.start() as u64,
            *MAX_CONNECTIONS.end() as u64,
            window,
            cx,
        );
        let parts_per_job = number(
            transfer.max_parts_per_job as u64,
            *MAX_PARTS_PER_JOB.start() as u64,
            *MAX_PARTS_PER_JOB.end() as u64,
            window,
            cx,
        );
        let skip_files = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("用逗号隔开")
                .default_value(transfer.skip_files.join(", "))
        });
        let edit_extensions = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("用逗号隔开")
                .default_value(edit.extensions.join(", "))
        });

        watch(&part_size, |t, v| t.part_size_mib = v as u64, cx);
        watch(&threshold, |t, v| t.multipart_threshold_mib = v as u64, cx);
        watch(&connections, |t, v| t.max_connections = v as u32, cx);
        watch(&parts_per_job, |t, v| t.max_parts_per_job = v as u32, cx);

        // 跳过列表是文本，单独订阅：按逗号拆开，空的丢掉
        cx.subscribe(&skip_files, |this, input, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let names: Vec<String> = input
                .read(cx)
                .value()
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            this.update_transfer(cx, |t| t.skip_files = names);
        })
        .detach();

        // 可编辑后缀：和跳过列表一样按逗号拆，顺手把用户随手写的点去掉
        cx.subscribe(&edit_extensions, |this, input, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let names: Vec<String> = input
                .read(cx)
                .value()
                .split(',')
                .map(|s| s.trim().trim_start_matches('.').to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            this.app.update(cx, |app, _| {
                app.config.edit.extensions = names;
                app.save_config();
            });
        })
        .detach();

        SettingsForm {
            part_size,
            threshold,
            connections,
            parts_per_job,
            skip_files,
            edit_extensions,
        }
    }
}

/// 带上下箭头的数字框，值夹在 `min..=max` 里。
fn number(
    value: u64,
    min: u64,
    max: u64,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .step(1.0)
            .min(min as f64)
            .max(max as f64)
            .default_value(value.to_string())
    })
}

/// 数字框改了就写回配置。解析不出来（框里正空着）就先不动。
fn watch(
    input: &Entity<InputState>,
    apply: impl Fn(&mut TransferConfig, f64) + 'static,
    cx: &mut Context<MainWindow>,
) {
    cx.subscribe(input, move |this, input, event: &InputEvent, cx| {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        let Ok(value) = input.read(cx).value().trim().parse::<f64>() else {
            return;
        };
        this.update_transfer(cx, |t| apply(t, value));
    })
    .detach();
}

impl MainWindow {
    /// 改一处传输设置：夹好、落盘、送给引擎。
    fn update_transfer(&mut self, cx: &mut Context<Self>, edit: impl FnOnce(&mut TransferConfig)) {
        self.app.update(cx, |app, _| {
            edit(&mut app.config.transfer);
            app.apply_transfer_config();
        });
        cx.notify();
    }

    pub fn render_settings(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.settings_form() else {
            return div().into_any_element();
        };
        let (part_size, threshold, connections, parts, skip, extensions) = (
            form.part_size.clone(),
            form.threshold.clone(),
            form.connections.clone(),
            form.parts_per_job.clone(),
            form.skip_files.clone(),
            form.edit_extensions.clone(),
        );
        let config = self.app.read(cx).config.clone();
        let t = &config.transfer;

        // 更新提示里点「查看」要能滚到「关于」，所以这里的滚动得由外面拿着句柄
        let scroll = self.settings_scroll.clone();
        v_flex()
            .id("settings")
            .size_full()
            .track_scroll(&scroll)
            .overflow_y_scroll()
            .p_6()
            .gap_6()
            .child(
                section("传输", cx)
                    .child(field(
                        "分片大小",
                        "大文件切成多大一片传。小于 5 MiB 服务端不收。",
                        NumberInput::new(&part_size).small().suffix(unit("MiB", cx)),
                        cx,
                    ))
                    .child(field(
                        "分片上传阈值",
                        "超过这个大小才走分片上传，小文件一次传完。",
                        NumberInput::new(&threshold).small().suffix(unit("MiB", cx)),
                        cx,
                    ))
                    .child(field(
                        "最大并发连接",
                        "同时开几条连接。调大传得快，也更容易被服务端限流。",
                        NumberInput::new(&connections).small(),
                        cx,
                    ))
                    .child(field(
                        "单任务并发分片",
                        "一个文件同时传几片。实际不会超过总连接数。",
                        NumberInput::new(&parts).small(),
                        cx,
                    ))
                    .child(toggle(
                        "download-md5",
                        "下载后校验 MD5",
                        "多花一遍读文件的时间，换一个确定没传坏。",
                        t.verify_download_md5,
                        cx.listener(|this, checked: &bool, _, cx| {
                            let checked = *checked;
                            this.update_transfer(cx, |t| t.verify_download_md5 = checked);
                        }),
                        cx,
                    ))
                    .child(toggle(
                        "auto-resume",
                        "启动时自动续传",
                        "上次没传完的任务，开程序就接着跑。",
                        t.auto_resume,
                        cx.listener(|this, checked: &bool, _, cx| {
                            let checked = *checked;
                            this.update_transfer(cx, |t| t.auto_resume = checked);
                        }),
                        cx,
                    ))
                    .child(field(
                        "上传时跳过",
                        "这些文件名不往上传，用逗号隔开。",
                        Input::new(&skip).small(),
                        cx,
                    )),
            )
            .child(
                section("界面", cx)
                    .child(field(
                        "配色",
                        "跟随系统的话，系统白天黑夜一变这儿就跟着变。",
                        h_flex().w_full().justify_end().child(
                            TabBar::new("theme")
                                .segmented()
                                .small()
                                .selected_index(match config.ui.theme {
                                    ThemeChoice::Light => 0,
                                    ThemeChoice::Dark => 1,
                                    ThemeChoice::System => 2,
                                })
                                .child(TabHead::new().label("浅色"))
                                .child(TabHead::new().label("深色"))
                                .child(TabHead::new().label("跟随系统"))
                                .on_click(cx.listener(|this, ix: &usize, window, cx| {
                                    let choice = match ix {
                                        0 => ThemeChoice::Light,
                                        1 => ThemeChoice::Dark,
                                        _ => ThemeChoice::System,
                                    };
                                    this.set_theme(choice, window, cx);
                                })),
                        ),
                        cx,
                    ))
                    .child(field(
                        "默认视图",
                        "新开的标签页用哪种视图。",
                        h_flex()
                            .w_full()
                            .gap_1()
                            .justify_end()
                            .child(
                                Button::new("settings-view-list")
                                    .label("列表")
                                    .ghost()
                                    .small()
                                    .selected(config.ui.view == ViewMode::List)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_view(ViewMode::List, cx)
                                    })),
                            )
                            .child(
                                Button::new("settings-view-icon")
                                    .label("图标")
                                    .ghost()
                                    .small()
                                    .selected(config.ui.view == ViewMode::Icon)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_view(ViewMode::Icon, cx)
                                    })),
                            ),
                        cx,
                    ))
                    .child(toggle(
                        "folders-first",
                        "文件夹排在前面",
                        "不管按哪一列排序，文件夹都在文件上面。",
                        config.ui.folders_first,
                        cx.listener(|this, checked: &bool, _, cx| {
                            this.set_folders_first(*checked, cx);
                        }),
                        cx,
                    )),
            )
            .child(
                section("编辑", cx)
                    .child(field(
                        "可编辑的后缀",
                        "这些后缀的文件能在右键菜单里「编辑」，用逗号隔开。",
                        Input::new(&extensions).small(),
                        cx,
                    ))
                    .child(toggle(
                        "edit-no-ext",
                        "没有后缀的也能编辑",
                        "Dockerfile、hosts 这类。注意没后缀的也可能是几个 G 的二进制。",
                        config.edit.no_extension,
                        cx.listener(|this, checked: &bool, _, cx| {
                            let checked = *checked;
                            this.app.update(cx, |app, _| {
                                app.config.edit.no_extension = checked;
                                app.save_config();
                            });
                            cx.notify();
                        }),
                        cx,
                    )),
            )
            .child(
                section("存储位置", cx)
                    .child(path_row(
                        "传输记录和日志",
                        "没传完的任务和日志放哪儿。换了要重启才生效——                         数据库连着、日志开着，中途换只会换出一半。",
                        data_dir().display().to_string(),
                        "data",
                        true,
                        cx,
                    ))
                    .child(path_row(
                        "配置文件",
                        "账户和设置。它得先被读出来才知道别的放哪儿，所以只能跟着程序走。",
                        config_path().display().to_string(),
                        "config",
                        false,
                        cx,
                    )),
            )
            .child(
                section("关于", cx)
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(div().text_sm().child("S3Browser"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("版本 {}", env!("CARGO_PKG_VERSION"))),
                            )
                            .children(self.latest_version_rows(cx))
                            .child(div().pt_1().child(self.update_buttons(cx))),
                    )
                    .child(toggle(
                        "check-updates",
                        "启动时检查更新",
                        "开程序时到 GitHub Releases 看一眼有没有新版本，有就在右下角提示。",
                        config.update.check_on_startup,
                        cx.listener(|this, checked: &bool, _, cx| {
                            let checked = *checked;
                            this.app.update(cx, |app, _| {
                                app.config.update.check_on_startup = checked;
                                app.save_config();
                            });
                            // 关了又开的话当场补查一次，省得干等下次启动
                            if checked
                                && this.update_status == s3browser::update::UpdateStatus::NotChecked
                            {
                                this.check_updates(true, cx);
                            }
                            cx.notify();
                        }),
                        cx,
                    )),
            )
            .vertical_scrollbar(&scroll)
            .into_any_element()
    }

    /// 「关于」里最新版本那一两行。没查过就什么都不显示，不平白占一行。
    fn latest_version_rows(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        use s3browser::update::UpdateStatus;

        let muted = |text: String| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(text)
                .into_any_element()
        };

        match &self.update_status {
            UpdateStatus::NotChecked => Vec::new(),
            // 「正在检查…」归按钮那一行说，这儿再来一遍就重了
            UpdateStatus::Checking => Vec::new(),
            UpdateStatus::UpToDate { latest } => {
                vec![muted(format!("最新版本 {latest}，已经是最新的了"))]
            }
            UpdateStatus::Failed(e) => vec![muted(format!("检查更新失败：{e}"))],
            UpdateStatus::Available(manifest) => {
                let mut rows = vec![
                    div()
                        .text_xs()
                        .text_color(cx.theme().link)
                        .child(match &manifest.published_at {
                            Some(date) => {
                                format!("最新版本 {}，{date} 发布", manifest.version)
                            }
                            None => format!("最新版本 {}", manifest.version),
                        })
                        .into_any_element(),
                ];
                if let Some(notes) = &manifest.notes {
                    if !notes.trim().is_empty() {
                        rows.push(muted(notes.trim().to_owned()));
                    }
                }
                rows
            }
        }
    }

    /// 「关于」里那排按钮，跟着检查结果和下载进度换脸：平时是「检查更新」，
    /// 有新版本就变成「立刻更新」和「立刻更新并重启」，下载中只剩一行进度。
    fn update_buttons(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use s3browser::update::UpdateStatus;

        let hint = |text: String, cx: &Context<Self>| {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(text)
        };
        let check = |label: &'static str, cx: &mut Context<Self>| {
            Button::new("update-check")
                .label(label)
                .outline()
                .xsmall()
                .on_click(cx.listener(|this, _, _, cx| this.check_updates(true, cx)))
        };

        match &self.update_phase {
            UpdatePhase::Downloading { done, total } => {
                let text = match total {
                    Some(total) => format!(
                        "正在下载… {}%（{} / {}）",
                        done.saturating_mul(100) / (*total).max(1),
                        format_size(*done),
                        format_size(*total)
                    ),
                    None => format!("正在下载… {}", format_size(*done)),
                };
                hint(text, cx).into_any_element()
            }
            UpdatePhase::Installed => h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(hint("新版已换上，重开一次就生效".into(), cx))
                .child(
                    Button::new("update-restart-now")
                        .label("现在重启")
                        .outline()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| this.restart_now(cx))),
                )
                .into_any_element(),
            UpdatePhase::Failed(e) => h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(hint(format!("上次没成：{e}"), cx))
                .child(check("重试", cx))
                .into_any_element(),
            UpdatePhase::Idle => match &self.update_status {
                UpdateStatus::Available(manifest) if manifest.url.is_some() => {
                    let size = manifest
                        .size
                        .map(|s| format!("（约 {}）", format_size(s)))
                        .unwrap_or_default();
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .child(
                            Button::new("update-now")
                                .label("立刻更新")
                                .outline()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _, cx| this.start_update(false, cx))),
                        )
                        .child(
                            Button::new("update-and-restart")
                                .label("立刻更新并重启")
                                .outline()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _, cx| this.start_update(true, cx))),
                        )
                        .when(!size.is_empty(), |this| this.child(hint(size, cx)))
                        .into_any_element()
                }
                UpdateStatus::Available(_) => h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(hint("这个发布里没有可下载的 exe，只能手动换".into(), cx))
                    .child(check("重新检查", cx))
                    .into_any_element(),
                UpdateStatus::Checking => hint("正在检查…".into(), cx).into_any_element(),
                // 按钮得有个 h_flex 兜着，不然它会被拉成整行宽
                _ => h_flex().child(check("检查更新", cx)).into_any_element(),
            },
        }
    }

    /// 「现在重启」：新版已经换好了，把它拉起来再退出自己。
    fn restart_now(&mut self, cx: &mut Context<Self>) {
        match std::env::current_exe().map(std::process::Command::new) {
            Ok(mut command) => match command.spawn() {
                Ok(_) => cx.quit(),
                Err(e) => self.toast_warn(format!("启动新版失败（{e}），手动重开吧"), cx),
            },
            Err(e) => self.toast_warn(format!("定位不到程序（{e}），手动重开吧"), cx),
        }
    }

    /// 挑一个新的数据目录。挑完只写进配置，不搬已有的文件——
    /// 传输记录和日志都是能重建的东西，搬过去反而可能搬坏。
    fn pick_data_dir(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("传输记录和日志放这里".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(dirs))) = picked.await else {
                return;
            };
            let Some(dir) = dirs.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.app.update(cx, |app, _| {
                    app.config.paths.data_dir = Some(dir);
                    app.save_config();
                });
                this.toast_ok("改好了，重启后生效", cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 回到跟配置文件同一个目录。
    fn reset_data_dir(&mut self, cx: &mut Context<Self>) {
        self.app.update(cx, |app, _| {
            app.config.paths.data_dir = None;
            app.save_config();
        });
        self.toast_ok("改回默认位置了，重启后生效", cx);
        cx.notify();
    }

    /// 在资源管理器里定位到这个路径。文件就选中它，目录就打开它。
    fn reveal_in_explorer(&mut self, path: &Path, cx: &mut Context<Self>) {
        let mut command = std::process::Command::new("explorer.exe");
        if path.is_dir() {
            command.arg(path);
        } else {
            command.arg("/select,").arg(path);
        }
        // explorer.exe 定位成功时也会返回非零退出码，所以只管发得出去发不出去
        if let Err(e) = command.spawn() {
            self.toast_err(format!("打不开资源管理器：{e}"), cx);
        }
    }

    /// 换配色。存进配置再应用，下次启动就是这个。
    fn set_theme(&mut self, choice: ThemeChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |app, _| {
            app.config.ui.theme = choice;
            app.save_config();
        });
        crate::apply_theme(choice, Some(window), cx);
        cx.notify();
    }

    /// 文件夹优先是个全局开关，所有打开的标签页一起改。
    fn set_folders_first(&mut self, value: bool, cx: &mut Context<Self>) {
        self.app.update(cx, |app, _| {
            app.config.ui.folders_first = value;
            app.save_config();
        });
        let states: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| tab.browser().map(|b| b.browser.clone()))
            .collect();
        for state in states {
            state.update(cx, |state, cx| {
                state.set_folders_first(value);
                cx.notify();
            });
        }
        self.refresh_table(cx);
        cx.notify();
    }
}

fn section(title: &'static str, cx: &App) -> Div {
    v_flex().gap_3().child(
        div()
            .text_sm()
            .font_bold()
            .text_color(cx.theme().muted_foreground)
            .child(title),
    )
}

/// 一行设置：左边标题和说明，右边控件。
fn field(
    title: &'static str,
    hint: &'static str,
    control: impl IntoElement,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_4()
        .items_start()
        .justify_between()
        .child(
            v_flex()
                .gap_0p5()
                .child(div().text_sm().child(title))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(hint),
                ),
        )
        .child(div().w(px(160.)).flex_shrink_0().child(control))
}

fn toggle(
    id: &'static str,
    title: &'static str,
    hint: &'static str,
    checked: bool,
    on_click: impl Fn(&bool, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    field(
        title,
        hint,
        h_flex()
            .w_full()
            .justify_end()
            .child(Switch::new(id).checked(checked).on_click(on_click)),
        cx,
    )
}

/// 一行路径：地址单独占一行铺满（路径长，跟 `field` 那样左右分会挤成一团），
/// 底下一排按钮。`changeable` 为真时多给一个"更改"，能挑新目录。
fn path_row(
    title: &'static str,
    hint: &'static str,
    path: String,
    id: &'static str,
    changeable: bool,
    cx: &mut Context<MainWindow>,
) -> impl IntoElement {
    let open_at = PathBuf::from(&path);
    v_flex()
        .w_full()
        .gap_1()
        .child(div().text_sm().child(title))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(hint),
        )
        .child(
            div()
                .w_full()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .text_xs()
                .child(path),
        )
        .child(
            h_flex()
                .w_full()
                .gap_1()
                .when(changeable, |this| {
                    this.child(
                        Button::new(SharedString::from(format!("{id}-change")))
                            .label("更改…")
                            .outline()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| this.pick_data_dir(cx))),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id}-reset")))
                            .label("恢复默认")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| this.reset_data_dir(cx))),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("{id}-open")))
                        .label("打开所在目录")
                        .ghost()
                        .xsmall()
                        .on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.reveal_in_explorer(&open_at, cx)
                            }),
                        ),
                ),
        )
}

fn unit(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .px_2()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}
