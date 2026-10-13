//! 一个位置：哪个账户的哪个桶的哪个目录。
//!
//! 单独成一个模块是因为两边都要它：`browser_state` 拿它当导航状态，
//! `config` 拿它当收藏项。放在 `browser_state` 里，config 就得反过来依赖它
//! （browser_state 已经依赖 config 的 `ViewMode`）；放进 config 又不对题——
//! config 管的是配置文件的读写。

use serde::{Deserialize, Serialize};

/// 一处位置：哪个账户的哪个桶的哪个目录。`prefix` 为空表示根，否则以 `/` 结尾。
///
/// **字段名就是 config.json 里收藏项的格式**（`Favorite` 把它平铺进去）。改名会让
/// 老配置里的收藏读不出来；以后加字段必须带 `#[serde(default)]`。老配置里多出来的
/// 字段（比如命名空间时代的 `root`/`label`）读时被忽略。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    pub profile: String,
    pub bucket: String,
    pub prefix: String,
}

impl Location {
    pub fn root(profile: impl Into<String>, bucket: impl Into<String>) -> Self {
        Location {
            profile: profile.into(),
            bucket: bucket.into(),
            prefix: String::new(),
        }
    }

    fn rebased(&self, prefix: String) -> Self {
        Location {
            profile: self.profile.clone(),
            bucket: self.bucket.clone(),
            prefix,
        }
    }

    /// 进入子目录。`name` 不带斜杠。
    pub fn child(&self, name: &str) -> Self {
        self.rebased(format!("{}{name}/", self.prefix))
    }

    /// 从桶根往下走一段相对路径，给"前往路径"用。
    ///
    /// 多余的斜杠直接合并掉；`..` 不解析——S3 的 key 里点点就是普通字符，
    /// 真按目录去理解反而会跳到用户没想去的地方。
    pub fn at(&self, path: &str) -> Self {
        let mut prefix = String::new();
        for segment in path.split('/').filter(|s| !s.is_empty() && *s != ".") {
            prefix.push_str(segment);
            prefix.push('/');
        }
        self.rebased(prefix)
    }

    /// 上一级。已经在桶根上时返回 None。
    pub fn parent(&self) -> Option<Self> {
        if self.prefix.is_empty() {
            return None;
        }
        let trimmed = self.prefix.trim_end_matches('/');
        let prefix = match trimmed.rfind('/') {
            Some(ix) => trimmed[..=ix].to_owned(),
            None => String::new(),
        };
        Some(self.rebased(prefix))
    }

    /// 桶根以下的各级目录名。
    fn relative_parts(&self) -> Vec<&str> {
        self.prefix
            .split('/')
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 面包屑用：桶名开头，后面是各级目录名。
    pub fn segments(&self) -> Vec<&str> {
        let mut out = vec![self.bucket.as_str()];
        out.extend(self.relative_parts());
        out
    }

    /// 截到第 n 段（0 是桶根）。
    pub fn truncated(&self, depth: usize) -> Self {
        let parts = self.relative_parts();
        let prefix = if depth == 0 {
            String::new()
        } else {
            let take = depth.min(parts.len());
            format!("{}/", parts[..take].join("/"))
        };
        self.rebased(prefix)
    }

    /// 目录下某个对象的完整 key。
    pub fn key_of(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_navigates_up_and_down() {
        let root = Location::root("p", "photos");
        assert_eq!(root.parent(), None);
        assert_eq!(root.segments(), vec!["photos"]);

        let sub = root.child("2026").child("09");
        assert_eq!(sub.prefix, "2026/09/");
        assert_eq!(sub.segments(), vec!["photos", "2026", "09"]);
        assert_eq!(sub.key_of("a.jpg"), "2026/09/a.jpg");

        let up = sub.parent().unwrap();
        assert_eq!(up.prefix, "2026/");
        assert_eq!(up.parent().unwrap().prefix, "");
        assert_eq!(up.parent().unwrap().parent(), None, "桶根没有上一级");
        assert_eq!(sub.truncated(1).prefix, "2026/");
        assert_eq!(sub.truncated(0).prefix, "");
    }

    /// 收藏项要落盘，前缀得原样回来。
    #[test]
    fn round_trips_through_json() {
        let at = Location::root("p", "photos").child("docs").child("2026");
        let text = serde_json::to_string(&at).unwrap();
        let back: Location = serde_json::from_str(&text).unwrap();

        assert_eq!(back, at);
    }

    /// 老配置里多出来的字段要能无声忽略。
    #[test]
    fn extra_fields_from_old_configs_are_ignored() {
        let text = r#"{"profile":"p","bucket":"b","prefix":"docs/","root":"","label":"b"}"#;
        let at: Location = serde_json::from_str(text).unwrap();

        assert_eq!(at, Location::root("p", "b").child("docs"));
    }
}
