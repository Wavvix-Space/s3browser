//! 检查更新和换新版。
//!
//! 更新信息来自 GitHub Releases：启动时查一次
//! `https://api.github.com/repos/<仓库>/releases/latest`，从最新发布里挑出
//! 新版的 exe：
//!
//! - `tag_name` 当版本号用（开头的 `v` 认不认都行）。
//! - 下载文件按名字找 `s3browser.exe`（不分大小写）；没有完全同名的，退而求
//!   其次取第一个 `.exe`。一个都没有就只提示不更新。
//! - 校验和按惯例找 `SHA256SUMS.txt`：每行 `<十六进制> <文件名>`。有就下完比
//!   一遍，没有就跳过。它防的是下半截和传坏，不是防篡改——发布没签名，
//!   仓库被人写了校验和也会跟着改。但这东西要拿去顶替正在跑的程序，
//!   换上去才发现是坏的就只剩一个启动不了的 exe，所以有的话值得比。
//! - `body` 是更新说明，`published_at` 是发布日期，都只显示给人看。
//!
//! 程序不做后台静默升级：换不换、要不要重启，都是用户在设置页「关于」里亲手按的。

use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

/// 查更新用的 GitHub 仓库，`<所有者>/<仓库名>`。
///
/// `S3B_UPDATE_REPO` 能盖掉它，指到自己的 fork 或本地起的服务试整套流程。
pub const GITHUB_REPO: &str = "Wavvix-Space/s3browser";

/// 发布里要挑的那个 exe 的文件名。
pub const EXE_ASSET: &str = "s3browser.exe";
/// 校验和文件的文件名，有就下、下完按行找。
const SHA_ASSET: &str = "sha256sums.txt";

/// 一次检查结果对应的发布信息。字段和设置页「关于」一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateManifest {
    pub version: String,
    /// 新版 exe 的下载地址。发布里没有可用的 exe 时是 None。
    pub url: Option<String>,
    /// 那个文件的 SHA-256，十六进制。大小写都认。
    pub sha256: Option<String>,
    /// 文件字节数，下载前拿来显示「约 27 MB」。
    pub size: Option<u64>,
    /// 更新说明，显示在设置页「关于」里。
    pub notes: Option<String>,
    /// 发布日期，原样显示，不解析。
    pub published_at: Option<String>,
}

/// 一次检查的结果。设置页「关于」那格照着它显示。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpdateStatus {
    /// 还没查过（关了检查，或者还没查完）。
    #[default]
    NotChecked,
    /// 正在查。
    Checking,
    /// 查过了，已经是最新。
    UpToDate { latest: String },
    /// 有新版本。
    Available(UpdateManifest),
    /// 想查但没查成，原因留着显示。不上浮层打扰，只在设置页留一行。
    Failed(String),
}

/// 有效的仓库地址。环境变量优先，都空就当没配。
pub fn update_repo() -> Option<String> {
    let from_env = std::env::var("S3B_UPDATE_REPO").unwrap_or_default();
    let repo = if from_env.trim().is_empty() {
        GITHUB_REPO.trim()
    } else {
        return Some(from_env.trim().to_owned());
    };
    (!repo.is_empty()).then(|| repo.to_owned())
}

/// GitHub 发布 JSON 里我们关心的那几项。
#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: Option<u64>,
}

/// 查最新发布并挑出要下载的东西。跑在 tokio 上——reqwest 离不开它。
pub async fn fetch_latest(repo: &str) -> Result<UpdateManifest, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    // GitHub 的 API 要 User-Agent，没带直接 403
    let bust = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let response = client
        .get(format!(
            "https://api.github.com/repos/{repo}/releases/latest?_={bust}"
        ))
        .header(reqwest::header::USER_AGENT, "s3browser")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status() == 404 {
        return Err(format!("仓库 {repo} 还没有发布过版本"));
    }
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    // reqwest 没开 json feature，拿字节自己解析
    let body = response.bytes().await.map_err(|e| e.to_string())?;
    let release: Release = serde_json::from_slice(&body).map_err(|e| {
        if body.starts_with(b"<") {
            "GitHub 回的是网页不是 JSON，多半是被限流或者地址写错了".to_owned()
        } else {
            e.to_string()
        }
    })?;

    let version = release.tag_name.trim().to_owned();
    if version.is_empty() {
        return Err("发布里没有版本号".into());
    }

    let exe = pick_exe_asset(&release.assets);
    let mut sha256 = None;
    if let (Some(exe), Some(sums)) = (exe.as_ref(), find_asset(&release.assets, SHA_ASSET)) {
        // 校验和文件读不成不拦路：大不了不校验，下回照旧比不过再算
        sha256 = download_text(&client, &sums.browser_download_url)
            .await
            .map(|text| parse_sha256sums(&text, &exe.name))
            .unwrap_or_else(|e| {
                tracing::warn!("读 {SHA_ASSET} 失败，跳过校验：{e}");
                None
            });
    }

    Ok(UpdateManifest {
        version,
        url: exe.as_ref().map(|a| a.browser_download_url.clone()),
        sha256,
        size: exe.as_ref().and_then(|a| a.size),
        notes: release.body.filter(|b| !b.trim().is_empty()),
        published_at: release
            .published_at
            .map(|t| t.split('T').next().unwrap_or(t.as_str()).to_owned()),
    })
}

/// 从发布文件里挑要下载的那个：先点名 `s3browser.exe`，再退到第一个 `.exe`。
fn pick_exe_asset(assets: &[Asset]) -> Option<&Asset> {
    find_asset(assets, EXE_ASSET).or_else(|| {
        assets.iter().find(|a| {
            let name = a.name.to_lowercase();
            name.ends_with(".exe") && name != format!("{EXE_ASSET}.old")
        })
    })
}

/// 按名字（不分大小写）找发布文件。
fn find_asset<'a>(assets: &'a [Asset], name: &str) -> Option<&'a Asset> {
    assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(name))
}

/// 解析 `SHA256SUMS.txt`：每行 `<十六进制> <文件名>`，找指定文件的那一行。
fn parse_sha256sums(text: &str, file_name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?;
        // 二进制模式校验出来的清单，文件名前面带一个 `*`（`hash  *file`）
        let name = name.trim_start_matches(['*', '#']);
        (name == file_name && !hash.is_empty()).then(|| hash.to_owned())
    })
}

/// 下一个小文本文件（校验和文件才几行，整个进内存无所谓）。
async fn download_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let body = response.bytes().await.map_err(|e| e.to_string())?;
    String::from_utf8(body.to_vec()).map_err(|e| e.to_string())
}

/// latest 是不是比 current 新。按点分段比数字，不按字典序——1.10.0 得比 1.9.0 大。
///
/// 解析不了的版本一律当「不比它新」：发布写坏了不该弹更新提示。
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(mut current), Some(mut latest)) => {
            // 段数不齐的补零：1.0 和 1.0.0 算同一个版本
            let len = current.len().max(latest.len());
            current.resize(len, 0);
            latest.resize(len, 0);
            latest > current
        }
        _ => false,
    }
}

/// "v1.2.3" -> [1, 2, 3]。任何一段不是纯数字就算整个解析失败。
fn parse_version(text: &str) -> Option<Vec<u64>> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let mut parts = Vec::new();
    for part in text.split('.') {
        parts.push(part.trim().parse::<u64>().ok()?);
    }
    (!parts.is_empty()).then_some(parts)
}

/// 下载中报进度用的：已下载字节，以及总字节（服务端没给 Content-Length 时为 None）。
pub type Progress = (u64, Option<u64>);

/// 把新版下到 `dest`，顺带算 SHA-256 并和发布里的校验和比对。
///
/// 进度从 `progress` 往外发，按百分比（没有总大小时按整 MiB）变了才发一条，
/// 免得刷爆界面。校验不过就把下坏的文件删掉再报错——留着它下次会被当成好的。
///
/// 跑在 tokio 上。
pub async fn download_update(
    url: &str,
    expected_sha256: Option<&str>,
    dest: &Path,
    progress: &async_channel::Sender<Progress>,
) -> Result<(), String> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let total = response.content_length();
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| e.to_string())?;

    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    let mut last_percent: u64 = 0;
    let mut last_mib: u64 = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        hasher.update(&chunk);
        done += chunk.len() as u64;
        let changed = match total {
            // 先乘后除，几十 MB 的文件在 64 位下不会溢出
            Some(total) => {
                let percent = done.saturating_mul(100) / total.max(1);
                percent != last_percent && {
                    last_percent = percent;
                    true
                }
            }
            None => {
                let mib = done / (1024 * 1024);
                mib != last_mib && {
                    last_mib = mib;
                    true
                }
            }
        };
        if changed {
            progress.send((done, total)).await.ok();
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);

    if let Some(expected) = expected_sha256 {
        let actual = hex::encode(hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            // 坏文件留着下次会被当成好的，删干净
            tokio::fs::remove_file(dest).await.ok();
            return Err(format!("校验不过：下到的文件 SHA-256 是 {actual}，发布里写的是 {expected}"));
        }
    }
    Ok(())
}

/// 把下好的新程序顶替掉当前这个 exe。成功时返回被换下来的旧程序的路径。
///
/// Windows 不许覆盖正在运行的 exe，但允许它改名挪开——所以是「旧的改名让位，
/// 新的顶上原名」。挪开失败（比如装在 Program Files 没写权限）就整个不做；
/// 新的没写进去也把旧名字还回来，不会把程序搞没。
///
/// 换下来的旧程序这会儿还在跑，删不掉，留给下次启动时的 [`sweep_old`]。
pub fn install(downloaded: &Path) -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("定位不到程序本体：{e}"))?;
    let dir = exe.parent().ok_or("程序没有所在目录")?;
    let retired = dir.join(OLD_EXE);

    // 上上次留下的，那会儿删不掉，现在试试
    let _ = std::fs::remove_file(&retired);
    std::fs::rename(&exe, &retired).map_err(|e| format!("旧程序挪不开：{e}"))?;

    if let Err(e) = std::fs::copy(downloaded, &exe) {
        // 新的没写成。copy 可能已经建了个半截文件，先删掉，
        // 否则 rename 会因为目标已存在而失败——那才是真把程序搞没了。
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::rename(&retired, &exe);
        return Err(format!("新程序写不进去：{e}"));
    }
    let _ = std::fs::remove_file(downloaded);
    Ok(retired)
}

/// 被换下来的旧程序叫什么。它和新的放在同一个目录里。
pub const OLD_EXE: &str = "s3browser.old.exe";

/// 把上次换下来的旧程序删掉。启动时调一次——那会儿它没在跑，删得掉。
///
/// 删不掉也无所谓（比如又有一个实例正开着它），下次启动接着试。
pub fn sweep_old() {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::fs::remove_file(dir.join(OLD_EXE));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_win() {
        assert!(is_newer("1.0.0", "1.0.1"));
        assert!(is_newer("1.0.0", "1.1.0"));
        assert!(is_newer("1.9.0", "1.10.0"));
        assert!(is_newer("1.0", "2.0"));
    }

    #[test]
    fn same_or_older_versions_lose() {
        assert!(!is_newer("1.0.1", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("2.0.0", "1.9.9"));
    }

    #[test]
    fn version_segments_pad_with_zero() {
        assert!(is_newer("1.0", "1.0.1"));
        assert!(!is_newer("1.0.0", "1.0"));
        assert!(!is_newer("1.0", "1.0"));
    }

    #[test]
    fn leading_v_is_tolerated() {
        assert!(is_newer("v1.0.0", "v1.0.1"));
        assert!(!is_newer("v1.0.1", "V1.0.0"));
    }

    #[test]
    fn garbage_never_looks_newer() {
        assert!(!is_newer("1.0.0", ""));
        assert!(!is_newer("1.0.0", "latest"));
        assert!(!is_newer("1.0.0", "1.x.0"));
        assert!(!is_newer("1.0.0", "1.0.0-beta"));
        assert!(!is_newer("", "1.0.0"));
    }

    /// 从 GitHub 的回包形状里挑对文件。
    #[test]
    fn picks_the_named_exe_then_any_exe() {
        let json = r#"{
            "tag_name": "v1.2.0",
            "assets": [
                {"name": "SHA256SUMS.txt", "browser_download_url": "https://e/sums"},
                {"name": "s3browser-ports.zip", "browser_download_url": "https://e/zip"},
                {"name": "s3browser.exe", "browser_download_url": "https://e/exe", "size": 27}
            ]
        }"#;
        let release: Release = serde_json::from_str(json).unwrap();
        let exe = pick_exe_asset(&release.assets).unwrap();
        assert_eq!(exe.name, "s3browser.exe");
        assert_eq!(exe.size, Some(27));
    }

    #[test]
    fn falls_back_to_any_exe() {
        let json = r#"{"tag_name":"1.2.0","assets":[
            {"name": "S3Browser.exe", "browser_download_url": "https://e/x"}
        ]}"#;
        let release: Release = serde_json::from_str(json).unwrap();
        assert_eq!(pick_exe_asset(&release.assets).unwrap().name, "S3Browser.exe");
    }

    #[test]
    fn a_release_without_an_exe_has_nothing_to_download() {
        let json = r#"{"tag_name":"1.2.0","assets":[
            {"name": "source.zip", "browser_download_url": "https://e/z"}
        ]}"#;
        let release: Release = serde_json::from_str(json).unwrap();
        assert!(pick_exe_asset(&release.assets).is_none());
    }

    /// 校验和文件的行格式：`<哈希> <文件名>`，哈希前可能带模式符。
    #[test]
    fn sha_sums_are_matched_by_file_name() {
        let text = "aaaa b.zip
bbbb  *s3browser.exe
cccc	S3Browser-old.exe
";
        // 文件名前的二进制模式符 `*` 要认
        assert_eq!(parse_sha256sums(text, "s3browser.exe").as_deref(), Some("bbbb"));
        assert_eq!(parse_sha256sums(text, "S3Browser-old.exe").as_deref(), Some("cccc"));
        assert!(parse_sha256sums(text, "missing.exe").is_none());
    }

    /// GitHub 的 API 字段多于我们读的，多出来的不能让它解析失败。
    #[test]
    fn release_ignores_everything_we_do_not_read() {
        let json = r#"{
            "tag_name": "v1.2.0", "id": 1, "draft": false, "prerelease": false,
            "url": "https://api.github.com/x", "assets": []
        }"#;
        let release: Release = serde_json::from_str(json).expect("多余字段该被忽略");
        assert_eq!(release.tag_name, "v1.2.0");
        assert!(release.assets.is_empty());
    }
}
