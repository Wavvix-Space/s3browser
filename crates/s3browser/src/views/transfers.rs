//! 传输视图。作为侧边栏的一个目的地，不是底部面板。
//!
//! 任务和历史都在一张表里：Table 自带虚拟滚动，几千行历史也只渲染可见的。
//! 进行中和历史共用列——同一件事的两个时间窗，列换了反而对不上。

use gpui_kit::component::tab::{Tab as TabHead, TabBar};
use gpui_kit::component::{button::*, table::*, *};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use transfer::job::{HistoryEntry, JobProgress, JobState};

use crate::views::main_window::MainWindow;
use s3browser::browser_state::format_size;

/// 传输页正在看哪一段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferView {
    /// 队列里跟着的任务：排队、传输、暂停、失败待重试、本次会话完成的。
    #[default]
    Active,
    /// 跨启动的历史，按落定时间倒序。
    History,
}

/// 引擎推过来的最新一份进度。
#[derive(Default)]
pub struct TransferQueue {
    pub jobs: Vec<JobProgress>,
    pub total_rate_bps: u64,
    pub history: Vec<HistoryEntry>,
    pub view: TransferView,
}

impl TransferQueue {
    pub fn active_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|j| matches!(j.state, JobState::Running | JobState::Queued))
            .count()
    }

    pub fn failed_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|j| j.state == JobState::Failed)
            .count()
    }
}

/// 传输表格。数据从 `TransferQueue` 现读，行序就是引擎给的顺序：
/// 新任务插队头，最新的永远在第一行。
pub struct TransferTable {
    queue: Entity<TransferQueue>,
    main: WeakEntity<MainWindow>,
    columns: Vec<Column>,
}

impl TransferTable {
    pub fn new(queue: Entity<TransferQueue>, main: WeakEntity<MainWindow>) -> Self {
        TransferTable {
            queue,
            main,
            columns: vec![
                Column::new("time", "时间").width(px(150.)).resizable(true),
                Column::new("kind", "类型").width(px(64.)).resizable(true),
                Column::new("name", "名称")
                    .width(px(280.))
                    .min_width(px(140.))
                    .resizable(true),
                Column::new("size", "大小").width(px(90.)).text_right().resizable(true),
                Column::new("progress", "进度").width(px(230.)).resizable(true),
                Column::new("state", "状态")
                    .width(px(160.))
                    .min_width(px(90.))
                    .resizable(true),
                Column::new("actions", "操作").width(px(140.)),
            ],
        }
    }

    fn job(&self, row_ix: usize, cx: &App) -> Option<JobProgress> {
        match self.queue.read(cx).view {
            TransferView::Active => self.queue.read(cx).jobs.get(row_ix).cloned(),
            TransferView::History => None,
        }
    }

    fn entry(&self, row_ix: usize, cx: &App) -> Option<HistoryEntry> {
        match self.queue.read(cx).view {
            TransferView::History => self.queue.read(cx).history.get(row_ix).cloned(),
            TransferView::Active => None,
        }
    }
}

impl TableDelegate for TransferTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, cx: &App) -> usize {
        let queue = self.queue.read(cx);
        match queue.view {
            TransferView::Active => queue.jobs.len(),
            TransferView::History => queue.history.len(),
        }
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        div().id(("transfer-row", row_ix))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if let Some(entry) = self.entry(row_ix, cx) {
            return self.render_history_td(entry, col_ix, cx).into_any_element();
        }
        match self.job(row_ix, cx) {
            Some(job) => self.render_job_td(job, col_ix, cx).into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let message = match self.queue.read(cx).view {
            TransferView::Active => "没有传输任务",
            TransferView::History => "没有历史记录",
        };
        h_flex()
            .size_full()
            .items_center()
            .justify_center()
            .text_color(cx.theme().muted_foreground)
            .child(message)
    }
}

impl TransferTable {
    fn render_job_td(
        &self,
        job: JobProgress,
        col_ix: usize,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        match col_ix {
            0 => format_transfer_time(job.created_at).into_any_element(),
            1 => kind_label(job.kind, cx).into_any_element(),
            2 => div()
                .truncate()
                .child(job.name.clone())
                .into_any_element(),
            3 => size_text(job.total).into_any_element(),
            4 => progress_cell(&job, cx).into_any_element(),
            5 => state_text(&job, cx).into_any_element(),
            _ => job_actions(job, self.main.clone()).into_any_element(),
        }
    }

    fn render_history_td(
        &self,
        entry: HistoryEntry,
        col_ix: usize,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        match col_ix {
            0 => format_transfer_time(entry.finished_at).into_any_element(),
            1 => kind_label(entry.kind, cx).into_any_element(),
            2 => div().truncate().child(entry.display_name().to_owned()).into_any_element(),
            3 => size_text(entry.size).into_any_element(),
            // 历史没有进度过程，只剩结果：一整条按结果上色的条
            4 => h_flex()
                .items_center()
                .child(progress_bar(1.0, &entry.state, entry.error.is_some(), cx))
                .into_any_element(),
            5 => match entry.state {
                JobState::Failed => div()
                    .truncate()
                    .text_color(cx.theme().red)
                    .child(entry.error.clone().unwrap_or_else(|| "失败".into()))
                    .into_any_element(),
                JobState::Cancelled => muted_text("已取消", cx).into_any_element(),
                _ => div()
                    .text_color(cx.theme().green)
                    .child("完成")
                    .into_any_element(),
            },
            _ => div().into_any_element(),
        }
    }
}

/// 类型格：小字标签。箭头图标在 16 像素下分不清上传下载，文字反而快。
fn kind_label(kind: transfer::job::JobKind, cx: &App) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(kind.label())
}

fn size_text(total: u64) -> String {
    if total == 0 {
        "--".to_owned()
    } else {
        format_size(total)
    }
}

fn muted_text(label: &str, cx: &App) -> impl IntoElement {
    div()
        .truncate()
        .text_color(cx.theme().muted_foreground)
        .child(label.to_owned())
}

fn state_text(job: &JobProgress, cx: &App) -> impl IntoElement {
    let label = if job.pausing {
        "正在暂停…".to_owned()
    } else {
        match job.state {
            JobState::Failed => job.error.clone().unwrap_or_else(|| "失败".to_owned()),
            JobState::Done => "完成".to_owned(),
            JobState::Paused => "已暂停".to_owned(),
            JobState::Cancelled => "已取消".to_owned(),
            JobState::Queued => "排队中".to_owned(),
            JobState::Running if job.total == 0 => format!("已处理 {} 项", job.transferred),
            JobState::Running => "传输中".to_owned(),
        }
    };
    let color = match job.state {
        JobState::Failed => cx.theme().red,
        JobState::Done => cx.theme().green,
        _ => cx.theme().muted_foreground,
    };
    div().truncate().text_color(color).child(label)
}

fn progress_bar(percent: f32, state: &JobState, failed: bool, cx: &App) -> impl IntoElement {
    let color = if failed || *state == JobState::Failed {
        cx.theme().red
    } else if *state == JobState::Done {
        cx.theme().green
    } else if state.is_terminal() {
        // 取消：灰着就行，没必要显眼
        cx.theme().muted_foreground
    } else {
        cx.theme().primary
    };
    div()
        .w(px(90.))
        .h(px(6.))
        .flex_shrink_0()
        .rounded_full()
        .bg(cx.theme().border)
        .child(div().h_full().w(relative(percent)).rounded_full().bg(color))
}

fn progress_cell(job: &JobProgress, cx: &App) -> impl IntoElement {
    let text = if job.total == 0 {
        String::new()
    } else if job.rate_bps > 0 && job.state == JobState::Running {
        format!(
            "{} / {} · {}/s",
            format_size(job.transferred),
            format_size(job.total),
            format_size(job.rate_bps)
        )
    } else {
        format!("{} / {}", format_size(job.transferred), format_size(job.total))
    };
    h_flex()
        .items_center()
        .gap_2()
        .child(progress_bar(job.percent(), &job.state, job.error.is_some(), cx))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .truncate()
                .child(text),
        )
}

fn job_actions(job: JobProgress, main: WeakEntity<MainWindow>) -> impl IntoElement {
    // 按了暂停但分片还没停下来时给"继续"：再点一下就是收回暂停，
    // 手上的分片接着跑，比按钮卡在"暂停"上装死强。
    let can_pause = matches!(job.state, JobState::Running | JobState::Queued) && !job.pausing;
    let can_resume = job.state == JobState::Paused || job.pausing;
    let can_retry = job.state == JobState::Failed;
    let done = job.state.is_terminal();

    h_flex()
        .flex_shrink_0()
        .gap_1()
        .when(can_pause, |this| {
            let main = main.clone();
            let id = job.id.clone();
            this.child(
                Button::new(SharedString::from(format!("pause-{}", job.id)))
                    .icon(IconName::Pause)
                    .ghost()
                    .small()
                    .on_click(move |_, _, cx| {
                        main.update(cx, |this, cx| this.app.read(cx).engine.pause(id.clone()))
                            .ok();
                    }),
            )
        })
        .when(can_resume, |this| {
            let main = main.clone();
            let id = job.id.clone();
            this.child(
                Button::new(SharedString::from(format!("resume-{}", job.id)))
                    .icon(IconName::Play)
                    .ghost()
                    .small()
                    .on_click(move |_, _, cx| {
                        main.update(cx, |this, cx| this.app.read(cx).engine.resume(id.clone()))
                            .ok();
                    }),
            )
        })
        .when(can_retry, |this| {
            let main = main.clone();
            let id = job.id.clone();
            this.child(
                Button::new(SharedString::from(format!("retry-{}", job.id)))
                    .icon(IconName::RotateCw)
                    .ghost()
                    .small()
                    .on_click(move |_, _, cx| {
                        main.update(cx, |this, cx| this.app.read(cx).engine.retry(id.clone()))
                            .ok();
                    }),
            )
        })
        .when(!done, |this| {
            let main = main.clone();
            let id = job.id.clone();
            this.child(
                Button::new(SharedString::from(format!("cancel-{}", job.id)))
                    .icon(IconName::CircleX)
                    .ghost()
                    .small()
                    .on_click(move |_, _, cx| {
                        main.update(cx, |this, cx| this.app.read(cx).engine.cancel(id.clone()))
                            .ok();
                    }),
            )
        })
}

/// 本地时区的时间。当天的只给时分秒，跨天的带上日期——
/// 历史拉长了看，日期才是定位用的那一位。
fn format_transfer_time(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "--".to_owned();
    }
    let Some(t) = chrono::DateTime::from_timestamp(timestamp, 0) else {
        return "--".to_owned();
    };
    let local = t.with_timezone(&chrono::Local);
    let now = chrono::Local::now();
    let pattern = if local.date_naive() == now.date_naive() {
        "%H:%M:%S"
    } else {
        "%Y-%m-%d %H:%M"
    };
    local.format(pattern).to_string()
}

impl MainWindow {
    pub fn render_transfers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let queue = self.queue.read(cx);
        let rate = queue.total_rate_bps;
        let failed = queue.failed_count();
        let view = queue.view;

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .h(px(40.))
                    .flex_shrink_0()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        TabBar::new("transfer-view")
                            .segmented()
                            .small()
                            .selected_index(match view {
                                TransferView::Active => 0,
                                TransferView::History => 1,
                            })
                            .child(TabHead::new().label("进行中"))
                            .child(TabHead::new().label("历史"))
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                let view = if *ix == 0 {
                                    TransferView::Active
                                } else {
                                    TransferView::History
                                };
                                this.queue.update(cx, |queue, cx| {
                                    queue.view = view;
                                    cx.notify();
                                });
                                this.transfer_table.update(cx, |_, cx| cx.notify());
                            })),
                    )
                    .child(
                        Button::new("pause-all")
                            .label("全部暂停")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.app.read(cx).engine.pause_all();
                            })),
                    )
                    .child(
                        Button::new("resume-all")
                            .label("全部继续")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.app.read(cx).engine.resume_all();
                            })),
                    )
                    .child(
                        Button::new("retry-failed")
                            .label(if failed > 0 {
                                format!("重试失败 ({failed})")
                            } else {
                                "重试失败".to_owned()
                            })
                            .ghost()
                            .small()
                            .disabled(failed == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.app.read(cx).engine.retry_failed();
                            })),
                    )
                    .child(
                        Button::new("clear-done")
                            .label("清除记录")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.app.read(cx).engine.clear_finished();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .when(rate > 0, |this| {
                                this.child(format!("{}/s", format_size(rate)))
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(DataTable::new(&self.transfer_table)),
            )
    }
}
