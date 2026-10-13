//! 文件浏览的状态机：位置、历史、条目、选择、排序、过滤。
//!
//! 这里是纯 Rust，不碰 GPUI，方便单测。视图只负责渲染它和把动作转发进来。

use std::cmp::Ordering;
use std::collections::HashSet;

use s3api::ObjectMeta;

pub use crate::config::ViewMode;
pub use crate::location::Location;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageClass {
    #[default]
    Standard,
    InfrequentAccess,
    Archive,
    DeepArchive,
    Other,
}

impl StorageClass {
    fn parse(raw: Option<&str>) -> Self {
        match raw.unwrap_or("") {
            "" | "STANDARD" | "STANDARD_IA" => StorageClass::Standard,
            "IA" | "ONEZONE_IA" | "INFREQUENT_ACCESS" => StorageClass::InfrequentAccess,
            "GLACIER" | "ARCHIVE" => StorageClass::Archive,
            "DEEP_ARCHIVE" | "COLD_ARCHIVE" => StorageClass::DeepArchive,
            _ => StorageClass::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StorageClass::Standard => "标准",
            StorageClass::InfrequentAccess => "低频",
            StorageClass::Archive => "归档",
            StorageClass::DeepArchive => "深度归档",
            StorageClass::Other => "其他",
        }
    }
}

/// 列表里的一行。只存文件名不存完整 key，十万条目下这点差别是几 MB。
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: Box<str>,
    pub size: u64,
    pub modified: i64,
    pub etag: Option<Box<str>>,
    pub storage_class: StorageClass,
    pub is_dir: bool,
}

/// 选中的一项，摘给 `ops` 里那些造任务的函数用。
///
/// 不直接传 `Entry`：造任务只关心这三样，拖着整行走会连 etag、
/// 存储类别一起复制，全选十万条目时那是白花的力气。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

impl From<&Entry> for Item {
    fn from(entry: &Entry) -> Self {
        Item {
            name: entry.name.to_string(),
            is_dir: entry.is_dir,
            size: entry.size,
        }
    }
}

impl Item {
    /// 测试和少数几处要现造一个。
    pub fn new(name: impl Into<String>, is_dir: bool, size: u64) -> Self {
        Item {
            name: name.into(),
            is_dir,
            size,
        }
    }
}

impl Entry {
    /// CommonPrefix 转成文件夹行。
    pub fn folder(prefix: &str, parent: &str) -> Option<Self> {
        let name = prefix
            .strip_prefix(parent)
            .unwrap_or(prefix)
            .trim_end_matches('/');
        if name.is_empty() {
            return None;
        }
        Some(Entry {
            name: name.into(),
            size: 0,
            modified: 0,
            etag: None,
            storage_class: StorageClass::Standard,
            is_dir: true,
        })
    }

    /// 对象转成文件行。作为目录占位符的那个空对象（key 就等于 prefix）会被丢掉。
    pub fn file(meta: &ObjectMeta, parent: &str) -> Option<Self> {
        let name = meta.key.strip_prefix(parent).unwrap_or(&meta.key);
        if name.is_empty() || name.ends_with('/') {
            return None;
        }
        Some(Entry {
            name: name.into(),
            size: meta.size,
            modified: meta.modified,
            etag: meta.etag.as_deref().map(Into::into),
            storage_class: StorageClass::parse(meta.storage_class.as_deref()),
            is_dir: false,
        })
    }

    /// 扩展名，小写，不含点。
    pub fn extension(&self) -> &str {
        if self.is_dir {
            return "";
        }
        match self.name.rfind('.') {
            Some(ix) if ix + 1 < self.name.len() => &self.name[ix + 1..],
            _ => "",
        }
    }

    pub fn kind_label(&self) -> String {
        if self.is_dir {
            return "文件夹".to_owned();
        }
        match self.extension() {
            "" => "文件".to_owned(),
            ext => format!("{} 文件", ext.to_uppercase()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Modified,
    Size,
    Kind,
}

#[derive(Debug, Default)]
pub struct BrowserState {
    location: Option<Location>,
    entries: Vec<Entry>,
    /// 过滤加排序后的 `entries` 下标。
    visible: Vec<u32>,
    filter: String,
    selection: HashSet<u32>,
    anchor: Option<u32>,
    sort_column: Option<SortColumn>,
    sort_desc: bool,
    folders_first: bool,
    view: ViewMode,
    back: Vec<Location>,
    forward: Vec<Location>,
    loading: bool,
    /// 还没拉完时的续页 token。
    next_token: Option<String>,
    error: Option<String>,
}

impl BrowserState {
    pub fn new(view: ViewMode, folders_first: bool) -> Self {
        BrowserState {
            sort_column: Some(SortColumn::Name),
            folders_first,
            view,
            ..Default::default()
        }
    }

    // ---------- 位置与历史 ----------

    pub fn location(&self) -> Option<&Location> {
        self.location.as_ref()
    }

    /// 跳到新位置并清空当前列表。会记进后退栈。
    pub fn navigate_to(&mut self, location: Location) {
        if self.location.as_ref() == Some(&location) {
            return;
        }
        if let Some(current) = self.location.take() {
            self.back.push(current);
        }
        self.forward.clear();
        self.enter(location);
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    pub fn go_back(&mut self) -> bool {
        let Some(previous) = self.back.pop() else {
            return false;
        };
        if let Some(current) = self.location.take() {
            self.forward.push(current);
        }
        self.enter(previous);
        true
    }

    pub fn go_forward(&mut self) -> bool {
        let Some(next) = self.forward.pop() else {
            return false;
        };
        if let Some(current) = self.location.take() {
            self.back.push(current);
        }
        self.enter(next);
        true
    }

    pub fn can_go_up(&self) -> bool {
        self.location.as_ref().is_some_and(|l| l.parent().is_some())
    }

    pub fn go_up(&mut self) -> bool {
        let Some(parent) = self.location.as_ref().and_then(Location::parent) else {
            return false;
        };
        self.navigate_to(parent);
        true
    }

    /// 重新加载当前目录，不动历史也不清搜索框。
    pub fn reload(&mut self, location: Location) {
        let filter = std::mem::take(&mut self.filter);
        self.enter(location);
        self.filter = filter;
    }

    /// 账户被删掉时把视图清空，回到"没选桶"的样子。
    pub fn close(&mut self) {
        self.location = None;
        self.entries = Vec::new();
        self.visible = Vec::new();
        self.selection = HashSet::new();
        self.anchor = None;
        self.back.clear();
        self.forward.clear();
        self.filter.clear();
        self.next_token = None;
        self.error = None;
        self.loading = false;
    }

    fn enter(&mut self, location: Location) {
        self.location = Some(location);
        // 换成新的容器，而不是 clear()：clear 会把容量留着，
        // 浏览过一个十万条目的目录之后那十几 MB 就再也还不回去了。
        self.entries = Vec::new();
        self.visible = Vec::new();
        self.selection = HashSet::new();
        self.anchor = None;
        self.filter.clear();
        self.next_token = None;
        self.error = None;
        self.loading = true;
    }

    // ---------- 加载 ----------

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn next_token(&self) -> Option<&str> {
        self.next_token.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
        self.loading = false;
        self.next_token = None;
    }

    /// 第一页：替换现有条目。
    pub fn set_page(&mut self, objects: &[ObjectMeta], prefixes: &[String], token: Option<String>) {
        self.entries.clear();
        self.selection.clear();
        self.anchor = None;
        self.push_page(objects, prefixes, token);
    }

    /// 后续页：追加。
    pub fn append_page(
        &mut self,
        objects: &[ObjectMeta],
        prefixes: &[String],
        token: Option<String>,
    ) {
        self.push_page(objects, prefixes, token);
    }

    fn push_page(&mut self, objects: &[ObjectMeta], prefixes: &[String], token: Option<String>) {
        let parent = self
            .location
            .as_ref()
            .map(|l| l.prefix.clone())
            .unwrap_or_default();
        self.entries
            .extend(prefixes.iter().filter_map(|p| Entry::folder(p, &parent)));
        self.entries
            .extend(objects.iter().filter_map(|o| Entry::file(o, &parent)));
        self.next_token = token;
        self.loading = self.next_token.is_some();
        self.error = None;
        self.rebuild();
    }

    // ---------- 条目 ----------

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 过滤排序后要显示的下标。
    pub fn visible(&self) -> &[u32] {
        &self.visible
    }

    pub fn visible_count(&self) -> usize {
        self.visible.len()
    }

    pub fn entry(&self, index: u32) -> Option<&Entry> {
        self.entries.get(index as usize)
    }

    /// 第 row 行对应的条目下标。
    pub fn row_index(&self, row: usize) -> Option<u32> {
        self.visible.get(row).copied()
    }

    /// 第 row 行显示的条目。
    pub fn row(&self, row: usize) -> Option<&Entry> {
        self.visible
            .get(row)
            .and_then(|ix| self.entries.get(*ix as usize))
    }

    pub fn total_size(&self) -> u64 {
        self.visible
            .iter()
            .filter_map(|ix| self.entries.get(*ix as usize))
            .filter(|e| !e.is_dir)
            .map(|e| e.size)
            .sum()
    }

    // ---------- 过滤与排序 ----------

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn set_filter(&mut self, filter: impl Into<String>) {
        self.filter = filter.into();
        self.rebuild();
    }

    pub fn sort(&self) -> (Option<SortColumn>, bool) {
        (self.sort_column, self.sort_desc)
    }

    /// 点列头：同一列切换升降序，换列则从升序开始。
    pub fn toggle_sort(&mut self, column: SortColumn) {
        let desc = self.sort_column == Some(column) && !self.sort_desc;
        self.set_sort(column, desc);
    }

    pub fn set_sort(&mut self, column: SortColumn, desc: bool) {
        self.sort_column = Some(column);
        self.sort_desc = desc;
        self.rebuild();
    }

    pub fn folders_first(&self) -> bool {
        self.folders_first
    }

    pub fn set_folders_first(&mut self, value: bool) {
        self.folders_first = value;
        self.rebuild();
    }

    pub fn view(&self) -> ViewMode {
        self.view
    }

    pub fn set_view(&mut self, view: ViewMode) {
        self.view = view;
    }

    fn rebuild(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| needle.is_empty() || e.name.to_lowercase().contains(&needle))
            .map(|(ix, _)| ix as u32)
            .collect();

        let Some(column) = self.sort_column else {
            return;
        };
        let entries = &self.entries;
        let desc = self.sort_desc;
        let folders_first = self.folders_first;
        self.visible.sort_by(|a, b| {
            let (a, b) = (&entries[*a as usize], &entries[*b as usize]);
            if folders_first && a.is_dir != b.is_dir {
                return if a.is_dir {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            let ordering = match column {
                SortColumn::Name => natural_cmp(&a.name, &b.name),
                SortColumn::Modified => a.modified.cmp(&b.modified),
                SortColumn::Size => a.size.cmp(&b.size),
                SortColumn::Kind => a
                    .extension()
                    .cmp(b.extension())
                    .then_with(|| natural_cmp(&a.name, &b.name)),
            };
            if desc { ordering.reverse() } else { ordering }
        });

        // 过滤后不再可见的条目不该继续算在选择里
        let visible: HashSet<u32> = self.visible.iter().copied().collect();
        self.selection.retain(|ix| visible.contains(ix));
    }

    // ---------- 选择 ----------

    pub fn is_selected(&self, index: u32) -> bool {
        self.selection.contains(&index)
    }

    pub fn selected_count(&self) -> usize {
        self.selection.len()
    }

    /// 选中项摘成 `Item`。界面那边要先断开对状态的借用才能接着改，
    /// 所以给一份拥有所有权的。
    pub fn selected_items(&self) -> Vec<Item> {
        self.selected_entries()
            .into_iter()
            .map(Item::from)
            .collect()
    }

    pub fn selected_entries(&self) -> Vec<&Entry> {
        self.visible
            .iter()
            .filter(|ix| self.selection.contains(ix))
            .filter_map(|ix| self.entries.get(*ix as usize))
            .collect()
    }

    /// 点某一行。`ctrl` 是加选，`shift` 是从锚点到这里的范围选。
    pub fn click_row(&mut self, row: usize, ctrl: bool, shift: bool) {
        let Some(&index) = self.visible.get(row) else {
            return;
        };
        match (ctrl, shift) {
            (_, true) => self.select_range_to(row),
            (true, false) => {
                if !self.selection.insert(index) {
                    self.selection.remove(&index);
                }
                self.anchor = Some(index);
            }
            (false, false) => {
                self.selection.clear();
                self.selection.insert(index);
                self.anchor = Some(index);
            }
        }
    }

    fn select_range_to(&mut self, row: usize) {
        let anchor_row = self
            .anchor
            .and_then(|a| self.visible.iter().position(|ix| *ix == a))
            .unwrap_or(row);
        let (from, to) = if anchor_row <= row {
            (anchor_row, row)
        } else {
            (row, anchor_row)
        };
        self.selection.clear();
        for ix in &self.visible[from..=to.min(self.visible.len().saturating_sub(1))] {
            self.selection.insert(*ix);
        }
    }

    /// 锚点所在的行号。方向键和快速预览都以它为"当前项"。
    pub fn focused_row(&self) -> Option<usize> {
        let anchor = self.anchor?;
        self.visible.iter().position(|ix| *ix == anchor)
    }

    /// 锚点指向的条目。没有锚点但只选中了一个时，用那一个。
    pub fn focused(&self) -> Option<&Entry> {
        if let Some(anchor) = self.anchor
            && let Some(entry) = self.entries.get(anchor as usize)
            && self.selection.contains(&anchor)
        {
            return Some(entry);
        }
        if self.selection.len() == 1 {
            return self.selected_entries().into_iter().next();
        }
        None
    }

    /// 只选中这一行，并把锚点挪过来。预览里按左右箭头切换时用。
    pub fn focus_row(&mut self, row: usize) -> Option<&Entry> {
        let &index = self.visible.get(row)?;
        self.selection.clear();
        self.selection.insert(index);
        self.anchor = Some(index);
        self.entries.get(index as usize)
    }

    pub fn select_all(&mut self) {
        self.selection = self.visible.iter().copied().collect();
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
        self.anchor = None;
    }

    /// 方向键移动。返回新的行号，供视图滚动到可见区。
    pub fn move_selection(&mut self, delta: isize, extend: bool) -> Option<usize> {
        if self.visible.is_empty() {
            return None;
        }
        let current = self
            .anchor
            .and_then(|a| self.visible.iter().position(|ix| *ix == a));
        let next = match current {
            Some(row) => (row as isize + delta).clamp(0, self.visible.len() as isize - 1) as usize,
            None if delta >= 0 => 0,
            None => self.visible.len() - 1,
        };
        if extend {
            self.select_range_to(next);
        } else {
            self.selection.clear();
            self.selection.insert(self.visible[next]);
            self.anchor = Some(self.visible[next]);
        }
        Some(next)
    }
}

/// 自然排序：`a2` 排在 `a10` 前面。大小写不敏感，和 Finder 一致。
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ac = a.chars().peekable();
    let mut bc = b.chars().peekable();
    loop {
        match (ac.peek().copied(), bc.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let xn = take_number(&mut ac);
                let yn = take_number(&mut bc);
                match xn.cmp(&yn) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
            (Some(x), Some(y)) => {
                ac.next();
                bc.next();
                let (x, y) = (x.to_lowercase().next(), y.to_lowercase().next());
                match x.cmp(&y) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
        }
    }
}

fn take_number(iter: &mut std::iter::Peekable<std::str::Chars<'_>>) -> u128 {
    let mut value: u128 = 0;
    while let Some(c) = iter.peek().copied() {
        if !c.is_ascii_digit() {
            break;
        }
        iter.next();
        // 超长数字串按饱和处理，不至于 panic
        value = value
            .saturating_mul(10)
            .saturating_add(c as u128 - '0' as u128);
    }
    value
}

/// 人类可读的大小，和 Finder 一样用十进制单位。
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.2} {}", UNITS[unit])
    } else if value < 100.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(key: &str, size: u64, modified: i64) -> ObjectMeta {
        ObjectMeta {
            key: key.to_owned(),
            size,
            modified,
            etag: Some("e".into()),
            storage_class: None,
            content_type: None,
        }
    }

    fn state_at(prefix: &str) -> BrowserState {
        let mut state = BrowserState::new(ViewMode::List, true);
        let mut location = Location::root("p", "b");
        location.prefix = prefix.into();
        state.navigate_to(location);
        state
    }

    #[test]
    fn entries_take_last_path_segment() {
        let folder = Entry::folder("2026/09/", "2026/").unwrap();
        assert_eq!(&*folder.name, "09");
        assert!(folder.is_dir);

        let file = Entry::file(&meta("2026/a.jpg", 10, 1), "2026/").unwrap();
        assert_eq!(&*file.name, "a.jpg");
        assert_eq!(file.extension(), "jpg");
        assert_eq!(file.kind_label(), "JPG 文件");

        // 目录占位对象不该出现在列表里
        assert!(Entry::file(&meta("2026/", 0, 1), "2026/").is_none());
        assert!(Entry::folder("2026/", "2026/").is_none());
    }

    #[test]
    fn folders_sort_before_files_by_default() {
        let mut state = state_at("");
        state.set_page(
            &[meta("b.txt", 1, 100), meta("a.txt", 2, 200)],
            &["zzz/".into()],
            None,
        );

        let names: Vec<&str> = (0..state.visible_count())
            .map(|r| &*state.row(r).unwrap().name)
            .collect();
        assert_eq!(names, vec!["zzz", "a.txt", "b.txt"]);
        assert!(!state.is_loading());
    }

    #[test]
    fn sorts_names_naturally() {
        let mut state = state_at("");
        state.set_folders_first(false);
        state.set_page(
            &[
                meta("file10.txt", 1, 1),
                meta("file2.txt", 1, 1),
                meta("File1.txt", 1, 1),
            ],
            &[],
            None,
        );

        let names: Vec<&str> = (0..state.visible_count())
            .map(|r| &*state.row(r).unwrap().name)
            .collect();
        assert_eq!(names, vec!["File1.txt", "file2.txt", "file10.txt"]);
    }

    #[test]
    fn toggling_sort_reverses_then_switches_column() {
        let mut state = state_at("");
        state.set_folders_first(false);
        state.set_page(
            &[meta("a", 30, 1), meta("b", 10, 2), meta("c", 20, 3)],
            &[],
            None,
        );

        state.toggle_sort(SortColumn::Name);
        assert_eq!(state.sort(), (Some(SortColumn::Name), true));
        assert_eq!(&*state.row(0).unwrap().name, "c");

        state.toggle_sort(SortColumn::Size);
        assert_eq!(state.sort(), (Some(SortColumn::Size), false));
        let sizes: Vec<u64> = (0..3).map(|r| state.row(r).unwrap().size).collect();
        assert_eq!(sizes, vec![10, 20, 30]);
    }

    #[test]
    fn filter_narrows_rows_and_drops_hidden_selection() {
        let mut state = state_at("");
        state.set_page(
            &[meta("report.pdf", 1, 1), meta("photo.jpg", 1, 1)],
            &[],
            None,
        );
        state.select_all();
        assert_eq!(state.selected_count(), 2);

        state.set_filter("PDF");

        assert_eq!(state.visible_count(), 1);
        assert_eq!(&*state.row(0).unwrap().name, "report.pdf");
        assert_eq!(state.selected_count(), 1);

        state.set_filter("");
        assert_eq!(state.visible_count(), 2);
    }

    #[test]
    fn click_selection_handles_ctrl_and_shift() {
        let mut state = state_at("");
        state.set_page(
            &[
                meta("a", 1, 1),
                meta("b", 1, 1),
                meta("c", 1, 1),
                meta("d", 1, 1),
            ],
            &[],
            None,
        );

        state.click_row(0, false, false);
        assert_eq!(state.selected_count(), 1);

        // ctrl 加选，再点一次取消
        state.click_row(2, true, false);
        assert_eq!(state.selected_count(), 2);
        state.click_row(2, true, false);
        assert_eq!(state.selected_count(), 1);

        // shift 从锚点连选
        state.click_row(0, false, false);
        state.click_row(3, false, true);
        assert_eq!(state.selected_count(), 4);

        // 反向也要工作
        state.click_row(3, false, false);
        state.click_row(1, false, true);
        assert_eq!(state.selected_count(), 3);

        state.clear_selection();
        assert_eq!(state.selected_count(), 0);
    }

    #[test]
    fn arrow_keys_move_and_extend_selection() {
        let mut state = state_at("");
        state.set_page(
            &[meta("a", 1, 1), meta("b", 1, 1), meta("c", 1, 1)],
            &[],
            None,
        );

        assert_eq!(state.move_selection(1, false), Some(0));
        assert_eq!(state.move_selection(1, false), Some(1));
        assert_eq!(state.selected_count(), 1);

        assert_eq!(state.move_selection(1, true), Some(2));
        assert_eq!(state.selected_count(), 2);

        // 到头了就停在边界
        assert_eq!(state.move_selection(5, false), Some(2));
        assert_eq!(state.move_selection(-9, false), Some(0));
    }

    #[test]
    fn history_walks_back_and_forward() {
        let mut state = BrowserState::new(ViewMode::List, true);
        let root = Location::root("p", "b");
        state.navigate_to(root.clone());
        state.navigate_to(root.child("one"));
        state.navigate_to(root.child("one").child("two"));

        assert!(state.can_go_back());
        assert!(!state.can_go_forward());

        assert!(state.go_back());
        assert_eq!(state.location().unwrap().prefix, "one/");
        assert!(state.can_go_forward());

        assert!(state.go_forward());
        assert_eq!(state.location().unwrap().prefix, "one/two/");

        // 新的导航会清掉前进栈
        state.go_back();
        state.navigate_to(root.child("other"));
        assert!(!state.can_go_forward());

        // 同一个位置重复导航不入栈
        let depth = state.back.len();
        state.navigate_to(root.child("other"));
        assert_eq!(state.back.len(), depth);
    }

    #[test]
    fn go_up_stops_at_bucket_root() {
        let mut state = state_at("a/b/");
        assert!(state.can_go_up());
        assert!(state.go_up());
        assert_eq!(state.location().unwrap().prefix, "a/");
        assert!(state.go_up());
        assert_eq!(state.location().unwrap().prefix, "");
        assert!(!state.can_go_up());
        assert!(!state.go_up());
    }

    #[test]
    fn paging_appends_and_tracks_loading() {
        let mut state = state_at("");
        state.set_page(&[meta("a", 1, 1)], &[], Some("tok".into()));
        assert!(state.is_loading());
        assert_eq!(state.next_token(), Some("tok"));

        state.append_page(&[meta("b", 1, 1)], &[], None);
        assert!(!state.is_loading());
        assert_eq!(state.visible_count(), 2);
    }

    #[test]
    fn navigating_resets_everything() {
        let mut state = state_at("");
        state.set_page(&[meta("a", 1, 1)], &[], None);
        state.select_all();
        state.set_filter("a");

        state.navigate_to(Location::root("p", "other"));

        assert_eq!(state.visible_count(), 0);
        assert_eq!(state.selected_count(), 0);
        assert_eq!(state.filter(), "");
        assert!(state.is_loading());
    }

    #[test]
    fn error_stops_loading() {
        let mut state = state_at("");
        state.set_error("连不上");
        assert!(!state.is_loading());
        assert_eq!(state.error(), Some("连不上"));
    }

    #[test]
    fn total_size_counts_files_only() {
        let mut state = state_at("");
        state.set_page(
            &[meta("a", 100, 1), meta("b", 200, 1)],
            &["dir/".into()],
            None,
        );
        assert_eq!(state.total_size(), 300);
    }

    #[test]
    fn formats_sizes_like_finder() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1000), "1.00 KB");
        assert_eq!(format_size(1_500_000), "1.50 MB");
        assert_eq!(format_size(15_000_000), "15.0 MB");
        assert_eq!(format_size(150_000_000), "150 MB");
        assert_eq!(format_size(2_000_000_000), "2.00 GB");
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;

    fn state_with(names: &[(&str, bool)]) -> BrowserState {
        let mut state = BrowserState::new(ViewMode::List, false);
        let objects: Vec<ObjectMeta> = names
            .iter()
            .filter(|(_, is_dir)| !is_dir)
            .map(|(name, _)| ObjectMeta {
                key: (*name).to_owned(),
                size: 1,
                modified: 0,
                etag: None,
                storage_class: None,
                content_type: None,
            })
            .collect();
        let prefixes: Vec<String> = names
            .iter()
            .filter(|(_, is_dir)| *is_dir)
            .map(|(name, _)| format!("{name}/"))
            .collect();
        state.navigate_to(Location::root("p", "b"));
        state.set_page(&objects, &prefixes, None);
        state
    }

    #[test]
    fn focus_row_selects_only_that_row() {
        let mut state = state_with(&[("a.txt", false), ("b.txt", false), ("c.txt", false)]);
        state.select_all();
        assert_eq!(state.selected_count(), 3);

        let name = state.focus_row(1).map(|e| e.name.to_string());
        assert_eq!(name.as_deref(), Some("b.txt"));
        assert_eq!(state.selected_count(), 1);
        assert_eq!(state.focused_row(), Some(1));
        assert_eq!(
            state.focused().map(|e| e.name.to_string()).as_deref(),
            Some("b.txt")
        );
    }

    #[test]
    fn focus_row_out_of_range_changes_nothing() {
        let mut state = state_with(&[("a.txt", false)]);
        state.focus_row(0);
        assert!(state.focus_row(9).is_none());
        assert_eq!(state.focused_row(), Some(0));
    }

    /// 多选时没有"当前项"，预览不该随便挑一个。
    #[test]
    fn focused_is_none_when_several_are_selected_without_an_anchor() {
        let mut state = state_with(&[("a.txt", false), ("b.txt", false)]);
        state.select_all();
        assert!(state.focused().is_none());
    }

    /// 只选了一个但锚点丢了（比如 select_all 后过滤掉了其他项），仍然算当前项。
    #[test]
    fn a_lone_selection_counts_as_focused() {
        let mut state = state_with(&[("alpha.txt", false), ("beta.txt", false)]);
        state.select_all();
        state.set_filter("alpha");
        assert_eq!(state.selected_count(), 1);
        assert_eq!(
            state.focused().map(|e| e.name.to_string()).as_deref(),
            Some("alpha.txt")
        );
    }

    /// 图标视图的上下键就是按列数整行跳。
    #[test]
    fn grid_movement_is_a_multi_row_step() {
        let mut state = state_with(&[
            ("a", false),
            ("b", false),
            ("c", false),
            ("d", false),
            ("e", false),
            ("f", false),
        ]);
        state.focus_row(0);
        // 每行 3 列，往下一行就是 +3
        assert_eq!(state.move_selection(3, false), Some(3));
        assert_eq!(
            state.focused().map(|e| e.name.to_string()).as_deref(),
            Some("d")
        );
        // 再往下越界，夹到最后一个
        assert_eq!(state.move_selection(3, false), Some(5));
    }
}
