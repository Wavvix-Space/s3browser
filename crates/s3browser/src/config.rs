//! `%APPDATA%\Wavvix\s3browser\config.json` 的读写。
//!
//! 凭据按明文存，这是自用工具的取舍：加密要么把密钥也放在旁边（等于没加），
//! 要么每次启动都问密码。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::buckets::BucketEntry;
use crate::location::Location;

/// 数据目录挂在厂商目录下面，免得和第三方软件 S3 Browser (s3browser.com) 撞名。
pub const APP_DIR: &str = "Wavvix/s3browser";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// 收藏，跨账户、有序。一条一条地读，坏一条丢一条，见 `lenient_favorites`。
    #[serde(default, deserialize_with = "lenient_favorites")]
    pub favorites: Vec<Favorite>,
    #[serde(default)]
    pub transfer: TransferConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub edit: EditConfig,
    #[serde(default)]
    pub update: UpdateConfig,
}

/// 一个账户：填 access key 的通用 S3 兼容服务。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub addressing: Addressing,
    #[serde(default)]
    pub insecure_tls: bool,
    #[serde(default)]
    pub payload_signing: PayloadSigning,
    /// 手动指定要显示的桶。为空时启动后调 ListBuckets。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buckets: Vec<String>,
    /// 桶在侧边栏里怎么排、哪些藏起来。跟 `buckets` 分工不同：
    /// 那个管"数据从哪来"，这个管"怎么显示"。
    #[serde(default, skip_serializing_if = "BucketView::is_empty")]
    pub view: BucketView,
}

/// 桶在侧边栏里怎么排、哪些藏起来。纯本地的事，不碰服务端。
///
/// 服务端返回什么是它的事——七牛这类服务会把所有地区的桶都列出来，
/// 而受地区限制实际能用的只有一个。这里管的就是把剩下的收起来。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BucketView {
    /// 排过序的桶 key。不在这里面的，按服务端返回的顺序缀在它们后面。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
    /// 本地藏起来的桶 key。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<String>,
}

impl BucketView {
    pub fn is_empty(&self) -> bool {
        self.order.is_empty() && self.hidden.is_empty()
    }

    pub fn is_hidden(&self, key: &str) -> bool {
        self.hidden.iter().any(|k| k == key)
    }

    /// 藏起来，已经藏着的就放出来。
    pub fn toggle_hidden(&mut self, key: &str) {
        if self.is_hidden(key) {
            self.hidden.retain(|k| k != key);
        } else {
            self.hidden.push(key.to_owned());
        }
    }

    /// 按本地顺序和可见性重排服务端返回的列表。
    ///
    /// 第二个返回值是"这一项是藏起来的"。`show_hidden` 为真时它们也留在结果里，
    /// 由界面画成半透明——不给条回头路的话，藏完就再也点不回来了。
    pub fn arrange<'a>(
        &self,
        entries: &'a [BucketEntry],
        show_hidden: bool,
    ) -> Vec<(&'a BucketEntry, bool)> {
        // key 现算要分配，这里每画一帧就跑一次，所以先算一遍、下面按下标取
        let keys: Vec<String> = entries.iter().map(BucketEntry::key).collect();
        // 排进去了没有。order 里重复点名同一个桶时靠它只排一次
        let mut placed = vec![false; entries.len()];
        let mut out = Vec::with_capacity(entries.len());
        // 排过序的先来。order 里点名了但服务端没返回的，跳过就是
        for key in &self.order {
            if let Some(ix) = keys.iter().position(|k| k == key)
                && !placed[ix]
            {
                placed[ix] = true;
                out.push((&entries[ix], self.is_hidden(key)));
            }
        }
        // 服务端有、order 里没有的缀在后面：新桶因此落在末尾
        for (ix, entry) in entries.iter().enumerate() {
            if !placed[ix] {
                out.push((entry, self.is_hidden(&keys[ix])));
            }
        }
        if !show_hidden {
            out.retain(|(_, hidden)| !hidden);
        }
        out
    }

    /// 眼前这份列表里藏着几个。
    ///
    /// 不是 `hidden.len()`：名单里可能躺着服务端早就不返回的 key（桶删了），
    /// 算进去的话菜单写着「(1)」，打开却什么也不多。
    pub fn hidden_count(&self, entries: &[BucketEntry]) -> usize {
        entries.iter().filter(|e| self.is_hidden(&e.key())).count()
    }

    /// 拖拽落下前用：把眼前的完整排列（含藏起来的）物化成一串 key。
    ///
    /// `order` 平时可能只覆盖一部分桶，"把第 3 个挪到第 1 位"在那种半拉子状态下
    /// 没有确定含义。物化一次之后就有了。
    pub fn full_order(&self, entries: &[BucketEntry]) -> Vec<String> {
        self.arrange(entries, true)
            .into_iter()
            .map(|(e, _)| e.key())
            .collect()
    }

    /// 把 `key` 那个桶挪到显示列表的第 `visible_slot` 条缝上。
    ///
    /// 两道换算都省不掉。`order` 平时可能只覆盖一部分桶，不先物化的话
    /// 「第 3 个挪到第 1 位」没有确定含义；而缝号是按**显示列表**数的，
    /// 藏起来的桶不在里面，得先换算到完整列表的坐标系去。
    pub fn move_to(
        &mut self,
        key: &str,
        visible_slot: usize,
        entries: &[BucketEntry],
        show_hidden: bool,
    ) {
        let mut order = self.full_order(entries);
        let visible: Vec<String> = self
            .arrange(entries, show_hidden)
            .into_iter()
            .map(|(e, _)| e.key())
            .collect();
        let Some(from) = order.iter().position(|k| k == key) else {
            return;
        };
        let slot = crate::reorder::slot_in_full(&visible, &order, visible_slot);
        crate::reorder::move_to_slot(&mut order, from, slot);
        self.order = order;
    }

    /// 只显示这一个，其余全藏起来。
    ///
    /// 给"拉回来二十个桶只有一个能用"准备的——右键十九次太蠢。
    pub fn only_show(&mut self, key: &str, entries: &[BucketEntry]) {
        self.hidden = entries
            .iter()
            .map(BucketEntry::key)
            .filter(|k| k != key)
            .collect();
    }
}

/// 收藏的一处位置：一个桶，或桶里的某一层目录。
///
/// 存的就是一个完整的 `Location`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Favorite {
    /// 指向哪儿。收藏的是桶时 `prefix` 为空。
    #[serde(flatten)]
    pub at: Location,
    /// 侧边栏上显示的名字。默认自动取，可以右键改。
    pub name: String,
}

impl Favorite {
    /// 从一个位置建收藏，名字自动取：桶用桶名，目录用最后一段。
    pub fn of(at: Location) -> Self {
        let name = at
            .segments()
            .last()
            .map(|s| (*s).to_owned())
            .unwrap_or_else(|| at.bucket.clone());
        Favorite { at, name }
    }

    /// 指的是一个桶的根，不是桶里的某层目录。界面据此选图标。
    pub fn is_bucket_root(&self) -> bool {
        self.at.prefix.is_empty()
    }

    /// 是不是同一处地方。只看账户、桶、目录前缀——名字是给人看的，改过名也还是那个地方。
    pub fn same_place(&self, at: &Location) -> bool {
        self.at.profile == at.profile && self.at.bucket == at.bucket && self.at.prefix == at.prefix
    }
}

/// 一条一条地读收藏，读不懂的记一笔日志然后丢掉。
///
/// 不能整列严格读：一条缺了字段，整份 Config 就解析失败，`Config::load` 会把
/// config.json 改名备份、换成默认配置——所有账户跟着一起没了。收藏又是最可能
/// 被人手改的一段。
///
/// 不给 `Location` 的字段加 `#[serde(default)]` 来兜：`prefix` 缺了默认成空串，
/// 会把一个目录收藏悄悄降级成桶根，指错地方还不如丢掉。
fn lenient_favorites<'de, D>(deserializer: D) -> Result<Vec<Favorite>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(favorite) => Some(favorite),
            Err(e) => {
                tracing::warn!("丢掉一条读不懂的收藏：{e}");
                None
            }
        })
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Addressing {
    #[default]
    Auto,
    VirtualHost,
    Path,
}

impl From<Addressing> for s3api::Addressing {
    fn from(a: Addressing) -> Self {
        match a {
            Addressing::Auto => s3api::Addressing::Auto,
            Addressing::VirtualHost => s3api::Addressing::VirtualHost,
            Addressing::Path => s3api::Addressing::Path,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PayloadSigning {
    #[default]
    Auto,
    Sha256,
}

impl From<PayloadSigning> for s3api::PayloadSigning {
    fn from(p: PayloadSigning) -> Self {
        match p {
            PayloadSigning::Auto => s3api::PayloadSigning::Auto,
            PayloadSigning::Sha256 => s3api::PayloadSigning::Sha256,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferConfig {
    pub part_size_mib: u64,
    pub multipart_threshold_mib: u64,
    pub max_connections: u32,
    pub max_parts_per_job: u32,
    pub verify_download_md5: bool,
    pub auto_resume: bool,
    /// 上传时跳过这些文件名。
    pub skip_files: Vec<String>,
}

/// 分片不能小于 5 MiB——S3 规定除最后一片外每片至少这么大，小了服务端回
/// EntityTooSmall。上限是防手滑：一片 1 GiB 意味着一次重试要重来 1 GiB。
pub const PART_SIZE_MIB: std::ops::RangeInclusive<u64> = 5..=1024;
/// 走分片上传的门槛。比这小的文件一次 PUT 完事。
pub const THRESHOLD_MIB: std::ops::RangeInclusive<u64> = 5..=4096;
/// 并发上限。几十条连接对单个服务端已经够了，再多只是更容易被限流。
pub const MAX_CONNECTIONS: std::ops::RangeInclusive<u32> = 1..=64;
/// 一个任务同时跑几片。
pub const MAX_PARTS_PER_JOB: std::ops::RangeInclusive<u32> = 1..=64;

impl TransferConfig {
    /// 把每个值夹回自己的合法范围。
    ///
    /// 只管单个字段的上下限，不管字段之间的关系（比如阈值该不该大于分片）。
    /// 那些留给 `AppState` 算引擎参数时取齐——设置界面上每个框是独立的，
    /// 在这里连带改别的字段，框里显示的值就和存下来的对不上了。
    pub fn clamped(mut self) -> Self {
        self.part_size_mib = self
            .part_size_mib
            .clamp(*PART_SIZE_MIB.start(), *PART_SIZE_MIB.end());
        self.multipart_threshold_mib = self
            .multipart_threshold_mib
            .clamp(*THRESHOLD_MIB.start(), *THRESHOLD_MIB.end());
        self.max_connections = self
            .max_connections
            .clamp(*MAX_CONNECTIONS.start(), *MAX_CONNECTIONS.end());
        self.max_parts_per_job = self
            .max_parts_per_job
            .clamp(*MAX_PARTS_PER_JOB.start(), *MAX_PARTS_PER_JOB.end());
        self
    }
}

impl Default for TransferConfig {
    fn default() -> Self {
        TransferConfig {
            part_size_mib: 8,
            multipart_threshold_mib: 16,
            max_connections: 8,
            max_parts_per_job: 4,
            verify_download_md5: true,
            auto_resume: true,
            skip_files: vec![".DS_Store".into(), "Thumbs.db".into(), "desktop.ini".into()],
        }
    }
}

/// 就地编辑：哪些文件能编辑、用什么打开。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditConfig {
    /// 这些后缀的文件能直接编辑，不区分大小写，带不带点都行。
    pub extensions: Vec<String>,
    /// 没有后缀的也能编辑。Dockerfile、hosts、README 这类挺常见，
    /// 但一个没后缀的文件也可能是几个 G 的二进制，所以单独给个开关。
    pub no_extension: bool,
    /// 用什么打开。留空就是记事本——它一定在，也一定能开文本。
    /// 想用别的就写命令，例如 `code -w`，文件路径会接在最后。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

impl Default for EditConfig {
    fn default() -> Self {
        EditConfig {
            extensions: [
                "txt",
                "md",
                "json",
                "log",
                "yaml",
                "yml",
                "toml",
                "ini",
                "conf",
                "csv",
                "xml",
                "sh",
                "ps1",
                "env",
                "properties",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            no_extension: true,
            command: None,
        }
    }
}

impl EditConfig {
    /// 这个名字的文件能不能编辑。
    pub fn allows(&self, name: &str) -> bool {
        match crate::file_kind::extension_of(name) {
            Some(ext) => self
                .extensions
                .iter()
                .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(ext)),
            None => self.no_extension,
        }
    }
}

/// 文件都放哪儿。
///
/// `config.json` 自己不在这里面——它得先被读出来才知道别的放哪儿，
/// 所以它的位置只认 `S3B_DATA_DIR` 环境变量和那个默认值。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathsConfig {
    /// 传输记录（transfers.db）和日志放哪儿。留空就跟 `config.json` 同一个目录。
    ///
    /// 改完要重启：数据库连着，日志文件开着，中途换地方只会换出一半。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
}

/// 检查更新。查哪个 GitHub 仓库是编译进程序的常量（`update::GITHUB_REPO`），
/// 这里只管查不查。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct UpdateConfig {
    /// 启动时到 GitHub Releases 看一眼有没有新版本。
    pub check_on_startup: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            check_on_startup: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub view: ViewMode,
    pub theme: Theme,
    pub folders_first: bool,
    pub sidebar_width: f32,
    pub window: WindowBox,
    /// 侧边栏「收藏」分组收起来了没。
    #[serde(default)]
    pub favorites_collapsed: bool,
    /// 侧边栏「全部」分组收起来了没。
    #[serde(default)]
    pub all_collapsed: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            view: ViewMode::List,
            theme: Theme::System,
            folders_first: true,
            sidebar_width: 220.0,
            window: WindowBox::default(),
            favorites_collapsed: false,
            all_collapsed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    List,
    Icon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowBox {
    pub width: f32,
    pub height: f32,
}

impl Default for WindowBox {
    fn default() -> Self {
        WindowBox {
            width: 1100.0,
            height: 700.0,
        }
    }
}

impl Config {
    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn profile_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    /// 新增或按 id 覆盖。
    pub fn upsert(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(slot) => *slot = profile,
            None => self.profiles.push(profile),
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.profiles.retain(|p| p.id != id);
        // 账户都没了，指着它的收藏点进去只会报错
        self.favorites.retain(|f| f.at.profile != id);
    }

    pub fn favorite_index(&self, at: &Location) -> Option<usize> {
        self.favorites.iter().position(|f| f.same_place(at))
    }

    /// 已收藏就取消，没收藏就加到末尾。返回值是"现在收藏着吗"。
    pub fn toggle_favorite(&mut self, at: &Location) -> bool {
        match self.favorite_index(at) {
            Some(ix) => {
                self.favorites.remove(ix);
                false
            }
            None => {
                self.favorites.push(Favorite::of(at.clone()));
                true
            }
        }
    }

    /// 读配置。文件不存在时给一份默认配置，损坏时把原文件改名备份后重来，
    /// 总之不能因为配置坏了就打不开程序。
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Config::default(),
        };
        match serde_json::from_str::<Config>(&text) {
            // 手改过的文件什么值都可能有，进门夹一次
            Ok(config) => Config {
                transfer: config.transfer.clamped(),
                ..config
            },
            Err(e) => {
                tracing::error!("配置解析失败，已备份为 config.json.bak：{e}");
                let _ = std::fs::rename(path, path.with_extension("json.bak"));
                Config::default()
            }
        }
    }

    /// 先写临时文件再改名，避免写一半断电留下半个配置。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

/// 生成一个短的账户 id，够本机用就行。
pub fn new_profile_id() -> String {
    format!("{:08x}", fastrand::u32(..))
}

/// `%APPDATA%\Wavvix\s3browser`，取不到时退回当前目录。
/// `config.json` 的家，也是没另外配置时数据的家。
///
/// `S3B_DATA_DIR` 能整个换掉，便携版拷着走就靠它。
pub fn base_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("S3B_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let root = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("Wavvix").join("s3browser")
}

pub fn config_path() -> PathBuf {
    base_dir().join("config.json")
}

/// 传输记录和日志放哪儿。配置里指了就用配置的。
///
/// 日志要在配置读出来之前就开始写（配置读坏了正是最需要日志的时候），
/// 所以这里自己读一遍配置，不等 `AppState`。读不出来就用默认位置。
pub fn data_dir() -> PathBuf {
    Config::load(&config_path())
        .paths
        .data_dir
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(base_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.gitignore` 这种整个名字就是个点开头的，是"没后缀"，
    /// 不是"后缀叫 gitignore"。名单里加不加 gitignore 都不该影响它。
    #[test]
    fn a_dotfile_counts_as_having_no_extension() {
        let mut edit = EditConfig::default();
        assert!(edit.allows(".gitignore"));
        assert!(edit.allows("Dockerfile"));
        edit.no_extension = false;
        assert!(!edit.allows(".gitignore"));
        assert!(!edit.allows("Dockerfile"));
        // 有后缀的不受这个开关影响
        assert!(edit.allows("a.txt"));
    }

    #[test]
    fn editable_extensions_ignore_case_and_leading_dots() {
        let edit = EditConfig {
            extensions: vec![".TXT".into(), "md".into()],
            no_extension: false,
            command: None,
        };
        assert!(edit.allows("a.txt"));
        assert!(edit.allows("a.TXT"));
        assert!(edit.allows("readme.MD"));
        assert!(!edit.allows("a.exe"));
        // 只看最后一段后缀
        assert!(edit.allows("notes.bak.md"));
        assert!(!edit.allows("notes.md.bak"));
    }

    /// 手改配置或者设置框里乱敲的值，进门就该被夹回来。
    #[test]
    fn transfer_settings_are_clamped_into_range() {
        let wild = TransferConfig {
            part_size_mib: 1,
            multipart_threshold_mib: 0,
            max_connections: 0,
            max_parts_per_job: 999,
            ..TransferConfig::default()
        }
        .clamped();

        // 5 MiB 是 S3 定的分片下限
        assert_eq!(wild.part_size_mib, 5);
        assert_eq!(wild.multipart_threshold_mib, 5);
        assert_eq!(wild.max_connections, 1);
        assert_eq!(wild.max_parts_per_job, 64);
    }

    #[test]
    fn a_sane_config_is_left_alone() {
        let ok = TransferConfig {
            part_size_mib: 16,
            multipart_threshold_mib: 64,
            max_connections: 8,
            max_parts_per_job: 4,
            ..TransferConfig::default()
        };
        assert_eq!(ok.clone().clamped().part_size_mib, ok.part_size_mib);
        assert_eq!(ok.clone().clamped().max_parts_per_job, ok.max_parts_per_job);
        assert_eq!(
            ok.clone().clamped().multipart_threshold_mib,
            ok.multipart_threshold_mib
        );
    }

    /// 夹值只管单个字段。字段之间的关系不在这儿动，不然设置框里显示的值
    /// 会和存下来的对不上。
    #[test]
    fn clamping_leaves_cross_field_relations_alone() {
        let odd = TransferConfig {
            part_size_mib: 64,
            // 阈值比分片小，是别扭但合法的值
            multipart_threshold_mib: 8,
            max_connections: 2,
            // 并发分片比连接数还多，同样别扭但合法
            max_parts_per_job: 8,
            ..TransferConfig::default()
        }
        .clamped();

        assert_eq!(odd.multipart_threshold_mib, 8);
        assert_eq!(odd.max_parts_per_job, 8);
    }

    #[test]
    fn the_default_config_is_already_in_range() {
        let d = TransferConfig::default();
        assert_eq!(d.clone().clamped().part_size_mib, d.part_size_mib);
        assert_eq!(d.clone().clamped().max_connections, d.max_connections);
    }

    fn sample() -> Config {
        Config {
            profiles: vec![
                Profile {
                    id: "aaaa".into(),
                    name: "本地".into(),
                    endpoint: "http://127.0.0.1:9000".into(),
                    region: "us-east-1".into(),
                    access_key: "ak".into(),
                    secret_key: "sk".into(),
                    addressing: Addressing::Path,
                    buckets: vec!["photos".into()],
                    view: BucketView::default(),
                    ..Default::default()
                },
                Profile {
                    id: "bbbb".into(),
                    name: "远程".into(),
                    endpoint: "https://s3.example.com".into(),
                    region: "us-east-1".into(),
                    access_key: "ak2".into(),
                    secret_key: "sk2".into(),
                    addressing: Addressing::VirtualHost,
                    buckets: Vec::new(),
                    view: BucketView::default(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_through_json() {
        let text = serde_json::to_string_pretty(&sample()).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();

        assert_eq!(back.profiles.len(), 2);
        assert_eq!(back.profiles[0].endpoint, "http://127.0.0.1:9000");
        assert_eq!(back.profiles[0].addressing, Addressing::Path);
        assert_eq!(back.profiles[1].endpoint, "https://s3.example.com");
        assert_eq!(back.profiles[1].addressing, Addressing::VirtualHost);
    }

    /// 老配置的账户带 `"type":"s3"` 这个类型标签（旧版本的账户配置遗留）。
    /// 标签现在没人读了，但认得出来，老配置不能因此读不出账户。
    #[test]
    fn old_tagged_profile_still_loads() {
        let text = r#"{"profiles":[{"id":"x","name":"n","type":"s3",
            "endpoint":"https://e","region":"r","access_key":"a","secret_key":"s"}]}"#;
        let config: Config = serde_json::from_str(text).unwrap();

        assert_eq!(config.profiles[0].endpoint, "https://e");
        assert_eq!(config.profiles[0].region, "r");
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let text = r#"{"profiles":[{"id":"x","name":"n","type":"s3","endpoint":"https://e"}]}"#;
        let config: Config = serde_json::from_str(text).unwrap();

        assert_eq!(config.transfer.part_size_mib, 8);
        assert_eq!(config.transfer.max_connections, 8);
        assert!(config.ui.folders_first);
        assert_eq!(config.profiles[0].addressing, Addressing::Auto);
        assert!(config.profiles[0].secret_key.is_empty());
    }

    #[test]
    fn upsert_replaces_by_id() {
        let mut config = sample();
        let mut updated = config.profiles[0].clone();
        updated.name = "改名了".into();
        config.upsert(updated);

        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.profile("aaaa").unwrap().name, "改名了");

        config.remove("aaaa");
        assert!(config.profile("aaaa").is_none());
    }

    #[test]
    fn save_then_load_keeps_content() {
        let dir = std::env::temp_dir().join(format!("s3b-cfg-{}", fastrand::u32(..)));
        let path = dir.join("config.json");
        sample().save(&path).unwrap();

        let loaded = Config::load(&path);
        assert_eq!(loaded.profiles.len(), 2);
        // 临时文件不该留下
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_config_is_backed_up_not_fatal() {
        let dir = std::env::temp_dir().join(format!("s3b-cfg-{}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{ 这不是 json").unwrap();

        let loaded = Config::load(&path);

        assert!(loaded.profiles.is_empty());
        assert!(path.with_extension("json.bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn entries(names: &[&str]) -> Vec<BucketEntry> {
        names.iter().map(|n| BucketEntry::bucket(*n)).collect()
    }

    fn labels(arranged: &[(&BucketEntry, bool)]) -> Vec<String> {
        arranged.iter().map(|(e, _)| e.bucket.clone()).collect()
    }

    fn view(order: &[&str], hidden: &[&str]) -> BucketView {
        BucketView {
            order: order.iter().map(|s| s.to_string()).collect(),
            hidden: hidden.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 没排过也没藏过的时候，服务端给什么顺序就是什么顺序。
    #[test]
    fn empty_view_keeps_server_order() {
        let view = BucketView::default();
        let list = entries(&["c", "a", "b"]);
        assert_eq!(labels(&view.arrange(&list, false)), ["c", "a", "b"]);
        assert!(view.is_empty());
    }

    /// order 里有的排在前面，order 里没有的按服务端顺序缀在后面。
    /// 新桶因此自动出现在末尾，不会插到你排好的序列中间。
    #[test]
    fn order_wins_and_new_buckets_go_last() {
        let view = view(&["b", "a"], &[]);
        let list = entries(&["a", "b", "zzz-new"]);
        assert_eq!(labels(&view.arrange(&list, false)), ["b", "a", "zzz-new"]);
    }

    /// order 里点名了、服务端却没返回的桶，跳过就是，不能 panic。
    #[test]
    fn stale_keys_in_order_are_skipped() {
        let view = view(&["gone", "b", "a"], &[]);
        let list = entries(&["a", "b"]);
        assert_eq!(labels(&view.arrange(&list, false)), ["b", "a"]);
    }

    /// order 是会被手改的配置，难免有重复的 key。重复了也只该出一行——
    /// 同一个桶在侧边栏里出现两次，拖哪个都说不清。
    #[test]
    fn duplicate_keys_in_order_are_listed_once() {
        let view = view(&["b", "a", "b"], &[]);
        let list = entries(&["a", "b"]);
        assert_eq!(labels(&view.arrange(&list, false)), ["b", "a"]);
    }

    /// 藏起来的桶平时不出现；打开"显示隐藏的桶"才回来，并且标着 hidden，
    /// 好让界面画成半透明——否则藏完就再也点不回来了。
    #[test]
    fn hidden_buckets_are_filtered_unless_asked_for() {
        let view = view(&[], &["b"]);
        let list = entries(&["a", "b", "c"]);

        assert_eq!(labels(&view.arrange(&list, false)), ["a", "c"]);

        let shown = view.arrange(&list, true);
        assert_eq!(labels(&shown), ["a", "b", "c"]);
        assert_eq!(
            shown.iter().map(|(_, h)| *h).collect::<Vec<_>>(),
            [false, true, false]
        );
        assert!(!view.is_empty());
    }

    /// 隐藏计数只算眼前列表里的。名单里过时的 key（桶已经没了）不算，
    /// 否则菜单写着「(1)」，打开却什么也不多。
    #[test]
    fn hidden_count_ignores_stale_keys() {
        let view = view(&[], &["b", "gone"]);
        assert_eq!(view.hidden_count(&entries(&["a", "b"])), 1);
    }

    #[test]
    fn toggling_hidden_twice_restores_it() {
        let mut view = BucketView::default();
        view.toggle_hidden("a");
        assert!(view.is_hidden("a"));
        view.toggle_hidden("a");
        assert!(!view.is_hidden("a"));
        assert!(view.is_empty(), "放出来之后不该留下空名单");
    }

    /// 拖拽落下前把完整排列物化成 order：隐藏的也在里面（取消隐藏后回原位），
    /// 服务端已经不返回的则在这时被清掉。
    #[test]
    fn full_order_materializes_everything_visible_and_hidden() {
        let view = view(&["gone", "c"], &["b"]);
        let list = entries(&["a", "b", "c"]);
        assert_eq!(view.full_order(&list), ["c", "a", "b"]);
    }

    /// 拖桶：缝号按眼前看到的行数，落下后 order 是完整的排列。
    #[test]
    fn moving_a_bucket_materializes_the_order() {
        let mut view = BucketView::default();
        let list = entries(&["a", "b", "c"]);
        // c 拖到最前
        view.move_to("c", 0, &list, false);
        assert_eq!(view.order, ["c", "a", "b"]);
        assert_eq!(labels(&view.arrange(&list, false)), ["c", "a", "b"]);
    }

    /// 藏起中间一个之后再拖，落点仍然和眼前看到的一致。不换算缝号的话，
    /// 每藏一个桶，拖拽就偏一格。
    #[test]
    fn moving_with_hidden_buckets_lands_where_it_looks() {
        let mut view = view(&[], &["b"]);
        let list = entries(&["a", "b", "c", "d"]);
        // 眼前是 a c d；把 a 拖到 c 和 d 中间，也就是显示列表的第 2 条缝
        view.move_to("a", 2, &list, false);
        assert_eq!(labels(&view.arrange(&list, false)), ["c", "a", "d"]);
        // 藏着的 b 也物化进了 order，放出来时有它自己的位置，不会窜到末尾
        assert_eq!(view.order, ["b", "c", "a", "d"]);
    }

    /// "只显示这个"：除了点中的，其余全进 hidden。
    #[test]
    fn only_show_hides_everything_else() {
        let mut view = BucketView::default();
        let list = entries(&["a", "b", "c"]);
        view.only_show("b", &list);

        assert_eq!(view.hidden, ["a", "c"]);
        assert_eq!(labels(&view.arrange(&list, false)), ["b"]);
    }

    /// 老配置没有 view 字段，读出来是空的；空的也不往回写，免得配置里多一堆 `{}`。
    #[test]
    fn empty_view_is_not_written_out() {
        let profile = serde_json::to_value(&sample().profiles[0]).unwrap();
        assert!(profile.get("view").is_none(), "{profile}");
    }

    /// 收藏一个桶：名字自动取根的显示名。
    #[test]
    fn favoriting_a_bucket_names_itself_after_the_bucket() {
        let fav = Favorite::of(Location::root("p", "photos"));
        assert_eq!(fav.name, "photos");
        assert!(fav.is_bucket_root());
    }

    /// 收藏桶里的一层目录：名字取最后一段，不是整条路径。
    #[test]
    fn favoriting_a_folder_names_itself_after_the_last_segment() {
        let at = Location::root("p", "photos").child("2026").child("交付");
        let fav = Favorite::of(at);
        assert_eq!(fav.name, "交付");
        assert!(!fav.is_bucket_root(), "目录不是桶根");
    }

    /// 判重只看账户+桶+目录。改过名的收藏照样认得出来，
    /// 不然改完名星标就空了，再点一下会收藏出第二份。
    #[test]
    fn toggle_finds_renamed_favorites() {
        let mut config = Config::default();
        let at = Location::root("p", "photos").child("2026");

        assert!(config.toggle_favorite(&at), "第一次是加进去");
        assert_eq!(config.favorites.len(), 1);
        config.favorites[0].name = "交付物".into();

        assert!(config.favorite_index(&at).is_some(), "改名不影响判重");
        assert!(!config.toggle_favorite(&at), "第二次是取消");
        assert!(config.favorites.is_empty());
    }

    /// 同名不同桶是两处地方。
    #[test]
    fn same_folder_name_in_different_buckets_are_distinct() {
        let mut config = Config::default();
        config.toggle_favorite(&Location::root("p", "one").child("logs"));
        config.toggle_favorite(&Location::root("p", "two").child("logs"));

        assert_eq!(config.favorites.len(), 2);
        assert_eq!(config.favorites[0].name, "logs");
        assert_eq!(config.favorites[1].name, "logs");
    }

    /// 删账户要连带清掉它的收藏，否则留下一堆点进去就报错的孤儿。
    #[test]
    fn removing_an_account_drops_its_favorites() {
        let mut config = sample();
        config.toggle_favorite(&Location::root("aaaa", "photos"));
        config.toggle_favorite(&Location::root("bbbb", "backup"));

        config.remove("aaaa");

        assert_eq!(config.favorites.len(), 1);
        assert_eq!(config.favorites[0].at.profile, "bbbb");
    }

    /// 收藏项要落盘并读回来，目录前缀得原样回来。
    #[test]
    fn favorites_round_trip_through_json() {
        let mut config = sample();
        let at = Location::root("bbbb", "data").child("inner").child("docs");
        config.toggle_favorite(&at);

        let text = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();

        assert_eq!(back.favorites.len(), 1);
        assert_eq!(back.favorites[0].at, at);
        assert_eq!(back.favorites[0].at.parent().unwrap().prefix, "inner/");
    }

    /// 用字面 JSON 锁住落盘格式：`Location` 的字段平铺在收藏项里，字段名就是这几个。
    /// 上面那个往返测试锁不住它——两边一起改名照样能过，可用户手上的老配置就读不出来了。
    /// 老收藏里多出来的 `root`/`label`（命名空间时代的遗留字段）要能无声忽略。
    #[test]
    fn favorite_format_is_flat() {
        let text = r#"{"favorites":[
            {"profile":"p","bucket":"b","prefix":"docs/","root":"","label":"b","name":"文档"}
        ]}"#;
        let config: Config = serde_json::from_str(text).unwrap();

        let fav = &config.favorites[0];
        assert_eq!(fav.at, Location::root("p", "b").child("docs"));
        assert_eq!(fav.name, "文档");
    }

    /// 坏一条只丢一条，不能连累整份配置。整份读不出来的话，`Config::load` 会把
    /// config.json 改名备份、换成默认配置——所有账户跟着一起没了。
    #[test]
    fn a_broken_favorite_is_dropped_not_fatal() {
        let text = r#"{
            "profiles": [{"id":"x","name":"n","type":"s3","endpoint":"https://e"}],
            "favorites": [
                {"profile":"x","bucket":"b","prefix":"","name":"好的"},
                {"profile":"x","prefix":"docs/","name":"缺了桶名"}
            ]
        }"#;
        let config: Config = serde_json::from_str(text).unwrap();

        assert_eq!(config.profiles.len(), 1, "账户不能跟着丢");
        assert_eq!(config.favorites.len(), 1);
        assert_eq!(config.favorites[0].name, "好的");
    }

    /// 老配置没有折叠状态，两个分组默认都展开。
    #[test]
    fn groups_start_expanded() {
        let text = r#"{"ui":{"view":"list","theme":"system","folders_first":true,
            "sidebar_width":220.0,"window":{"width":1100.0,"height":700.0}}}"#;
        let config: Config = serde_json::from_str(text).unwrap();
        assert!(!config.ui.favorites_collapsed);
        assert!(!config.ui.all_collapsed);
    }
}
