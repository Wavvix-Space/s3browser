//! 侧边栏里一个账户下挂的桶。
//!
//! 放在 lib 而不是界面那边，是为了按本地顺序和隐藏名单重排它们的逻辑
//! （`config::BucketView`）能写成纯函数单测。

use crate::location::Location;

/// 侧边栏里一个账户下的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketEntry {
    /// 真实桶名。
    pub bucket: String,
}

impl BucketEntry {
    pub fn bucket(name: impl Into<String>) -> Self {
        BucketEntry {
            bucket: name.into(),
        }
    }

    /// 顺序和隐藏名单拿它当键。
    ///
    /// **它会原样落进 config.json**（`BucketView` 的 `order` 和 `hidden`）。
    /// 格式一改，所有人存下的顺序和隐藏名单就悄悄对不上了。
    pub fn key(&self) -> String {
        self.bucket.clone()
    }

    /// 点开这一项去哪儿。
    pub fn location(&self, profile: &str) -> Location {
        Location::root(profile, self.bucket.clone())
    }
}

/// 侧边栏里一个账户的桶列表。
#[derive(Debug, Clone, Default)]
pub struct BucketList {
    pub loading: bool,
    pub buckets: Vec<BucketEntry>,
    pub error: Option<String>,
    /// 展开过一次就不再自动重拉。
    pub loaded: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_entries_key_by_their_name() {
        let entries: Vec<BucketEntry> = ["photos", "backup"]
            .iter()
            .map(|n| BucketEntry::bucket(*n))
            .collect();

        assert_eq!(entries[0].bucket, "photos");
        assert_eq!(entries[1].key(), "backup");
    }

    /// 点进去要落在桶根。
    #[test]
    fn entries_open_at_the_bucket_root() {
        let entry = BucketEntry::bucket("photos");
        let at = entry.location("p");
        assert_eq!(at, Location::root("p", "photos"));
        assert_eq!(at.parent(), None, "桶根没有上一级");
    }
}
