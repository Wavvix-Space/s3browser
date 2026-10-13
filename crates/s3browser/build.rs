//! 把应用图标塞进 exe 的资源段。
//!
//! GPUI 在 Windows 上取窗口图标的办法是 `LoadImageW(模块句柄, 资源号 1)`，
//! `WindowOptions` 里没留设置图标的口子。所以只要把 .ico 挂成 1 号资源，
//! 标题栏、任务栏、Alt+Tab，外加资源管理器里 exe 自己的图标，一起就都有了。
//!
//! 不用 winresource / embed-resource：那两个干的也是「找资源编译器、生成
//! .res、加条链接参数」这三步，自己写看得见摸得着，也少一个依赖。
//!
//! 找不到资源编译器就只警告不报错——少个图标而已，不该让整个构建垮掉。

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 资源号写死 1，GPUI 就按这个号找。
const ICON_RESOURCE_ID: u16 = 1;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let icon = manifest.join("assets").join("app.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    if !icon.exists() {
        warn(&format!("没有 {}，exe 不带图标", icon.display()));
        return;
    }

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let rc = out.join("icon.rc");
    let res = out.join("icon.res");

    // .rc 里的路径走正斜杠：反斜杠在这儿是转义符，要么翻倍要么换向，换向省事
    let script = format!(
        "{ICON_RESOURCE_ID} ICON \"{}\"\n",
        icon.display().to_string().replace('\\', "/")
    );
    std::fs::write(&rc, script).expect("写 icon.rc");

    let Some(compiler) = resource_compiler() else {
        warn("PATH 上没有 llvm-rc，Windows SDK 里也没找到 rc.exe，exe 不带图标");
        return;
    };
    match Command::new(&compiler)
        .arg("/fo")
        .arg(&res)
        .arg(&rc)
        .status()
    {
        Ok(status) if status.success() => {
            // 只挂到 bin 上。lib 是给单测用的，链接参数加上去反而会让测试构建报错
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        Ok(status) => warn(&format!(
            "{} 退出码 {status}，exe 不带图标",
            compiler.display()
        )),
        Err(e) => warn(&format!(
            "跑不起来 {}：{e}，exe 不带图标",
            compiler.display()
        )),
    }
}

/// 找个资源编译器。
///
/// 先试 `llvm-rc`——装 Rust 的 MSVC 工具链时一般跟着来了，而且在 PATH 上。
/// 没有就去 Windows SDK 里翻 `rc.exe`，SDK 按版本号分目录，取版本号最大那个。
fn resource_compiler() -> Option<PathBuf> {
    let llvm_rc = PathBuf::from("llvm-rc");
    if Command::new(&llvm_rc).arg("/?").output().is_ok() {
        return Some(llvm_rc);
    }

    let roots = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    let mut versions: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| std::fs::read_dir(Path::new(root)).ok())
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.join("x64").join("rc.exe").exists())
        .collect();
    // 目录名就是 SDK 版本号，字典序在这儿正好等于版本序（都是 10.0.x.0）
    versions.sort();
    versions.pop().map(|path| path.join("x64").join("rc.exe"))
}

fn warn(message: &str) {
    println!("cargo:warning={message}");
}
