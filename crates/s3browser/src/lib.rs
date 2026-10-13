//! s3browser 的纯逻辑部分：配置、浏览状态、任务构造。
//!
//! 界面在 `main.rs` 那个 bin 里。分开是因为把 GPUI 的长链式构建器和单测一起编译
//! 会让 rustc 栈溢出，而且这些逻辑本来就不该依赖界面。

pub mod browser_state;
pub mod buckets;
pub mod config;
pub mod file_kind;
pub mod location;
pub mod logging;
pub mod ops;
pub mod preview;
pub mod reorder;
pub mod tabs;
pub mod update;
