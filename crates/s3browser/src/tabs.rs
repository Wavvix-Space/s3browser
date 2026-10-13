//! 标签页的落点规则。
//!
//! 标签条上除了打开的桶，还有传输和设置——它们不常驻，点了才开，开了也能关。
//! "点一个桶之后落到哪一格"于是有一套确定的规矩，这里只有规矩，不碰 GPUI。

use crate::browser_state::Location;

/// 标签条上的一格，从落点规则的角度看只有三种。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// 打开着某个桶。
    Bucket(Location),
    /// 空白的浏览页，可以直接拿来用。
    Blank,
    /// 传输、设置这类。不是浏览页，不能拿来开桶。
    Other,
}

/// 点一个桶之后该落到哪儿。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabTarget {
    /// 这个桶已经开着了，切过去就行，别开第二份。
    Focus(usize),
    /// 当前这页还是空的（刚启动或刚关完），拿来用。
    Reuse(usize),
    /// 开新的一页。
    New,
}

pub fn pick_tab(slots: &[Slot], active: usize, want: &Location) -> TabTarget {
    if let Some(ix) = slots.iter().position(|slot| match slot {
        Slot::Bucket(open) => same_bucket(open, want),
        _ => false,
    }) {
        return TabTarget::Focus(ix);
    }
    // 只认当前这页的空白，别去别处抢——那会让标签顺序跳来跳去
    if slots.get(active) == Some(&Slot::Blank) {
        return TabTarget::Reuse(active);
    }
    TabTarget::New
}

/// 两个位置算不算同一个"桶"。
///
/// `prefix` 不比，进了子目录还是同一页。
fn same_bucket(a: &Location, b: &Location) -> bool {
    a.profile == b.profile && a.bucket == b.bucket
}

/// 右键标签页时那几个"批量关"的范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseScope {
    /// 除了这一格，剩下的全关。
    Others,
    /// 这一格左边的全关。
    Left,
    /// 这一格右边的全关。
    Right,
    /// 一格不留。
    All,
}

/// 按范围算出要关掉哪几格，**从大到小**给。
///
/// 倒序不是随手排的：删掉一格，排在它后面的全往前挪一位，手上攒着的索引
/// 就全歪了。从后往前删没这回事，所以顺序在这儿排好，省得每个调用点
/// 自己记着。
pub fn tabs_to_close(count: usize, target: usize, scope: CloseScope) -> Vec<usize> {
    (0..count)
        .rev()
        .filter(|&ix| match scope {
            CloseScope::Others => ix != target,
            CloseScope::Left => ix < target,
            CloseScope::Right => ix > target,
            CloseScope::All => true,
        })
        .collect()
}

/// 关掉第 `closing` 页之后焦点落在哪一页。
///
/// 规矩和浏览器一样：关当前页，右边那页顶上来；右边没有了就往左退一格。
/// 关别的页，当前页跟着走，位置不变。
pub fn active_after_close(count: usize, active: usize, closing: usize) -> usize {
    let left = count.saturating_sub(1);
    if left == 0 {
        return 0;
    }
    let next = if closing < active { active - 1 } else { active };
    next.min(left - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(profile: &str, name: &str) -> Slot {
        Slot::Bucket(Location::root(profile, name))
    }

    fn loc(profile: &str, name: &str) -> Location {
        Location::root(profile, name)
    }

    #[test]
    fn an_open_bucket_just_gets_focused() {
        let slots = vec![bucket("p1", "a"), bucket("p1", "b")];
        assert_eq!(pick_tab(&slots, 0, &loc("p1", "b")), TabTarget::Focus(1));
    }

    /// 进了子目录还是同一页，不该再开一份。
    #[test]
    fn a_subdirectory_still_counts_as_the_same_tab() {
        let deep = loc("p1", "a").child("photos").child("2026");
        let slots = vec![Slot::Bucket(deep)];
        assert_eq!(pick_tab(&slots, 0, &loc("p1", "a")), TabTarget::Focus(0));
    }

    /// 同名的桶挂在两个账户下是两回事。
    #[test]
    fn the_same_bucket_name_on_another_account_is_a_new_tab() {
        let slots = vec![bucket("p1", "shared")];
        assert_eq!(pick_tab(&slots, 0, &loc("p2", "shared")), TabTarget::New);
    }

    /// 刚启动只有一张空白页，第一个桶就开在它上面，不该冒出第二页。
    #[test]
    fn the_blank_tab_gets_reused() {
        assert_eq!(
            pick_tab(&[Slot::Blank], 0, &loc("p1", "a")),
            TabTarget::Reuse(0)
        );
    }

    /// 当前页有东西，就算别处有空白页也照样新开——不然标签会莫名其妙换位置。
    #[test]
    fn a_blank_tab_elsewhere_is_left_alone() {
        let slots = vec![Slot::Blank, bucket("p1", "a")];
        assert_eq!(pick_tab(&slots, 1, &loc("p1", "b")), TabTarget::New);
    }

    /// 站在传输页上点桶，不能把传输页顶掉——它不是空白浏览页。
    #[test]
    fn the_transfers_tab_is_never_taken_over() {
        let slots = vec![bucket("p1", "a"), Slot::Other];
        assert_eq!(pick_tab(&slots, 1, &loc("p1", "b")), TabTarget::New);
        // 已经开着的桶还是该切过去，不受当前站在哪一页影响
        assert_eq!(pick_tab(&slots, 1, &loc("p1", "a")), TabTarget::Focus(0));
    }

    #[test]
    fn closing_the_current_tab_pulls_in_the_one_on_its_right() {
        assert_eq!(active_after_close(3, 1, 1), 1);
    }

    #[test]
    fn closing_the_last_tab_falls_back_to_its_left() {
        assert_eq!(active_after_close(3, 2, 2), 1);
    }

    #[test]
    fn closing_a_tab_on_the_left_keeps_the_current_one() {
        assert_eq!(active_after_close(3, 2, 0), 1);
    }

    #[test]
    fn closing_a_tab_on_the_right_changes_nothing() {
        assert_eq!(active_after_close(3, 0, 2), 0);
    }

    /// 关掉最后一页会补一张空白页，索引得是 0。
    #[test]
    fn closing_the_only_tab_lands_on_zero() {
        assert_eq!(active_after_close(1, 0, 0), 0);
    }

    #[test]
    fn closing_the_others_spares_only_the_target() {
        assert_eq!(tabs_to_close(4, 1, CloseScope::Others), vec![3, 2, 0]);
    }

    #[test]
    fn closing_one_side_leaves_the_other_alone() {
        assert_eq!(tabs_to_close(4, 2, CloseScope::Left), vec![1, 0]);
        assert_eq!(tabs_to_close(4, 2, CloseScope::Right), vec![3]);
    }

    /// 已经在最左边那格上了，"关闭左侧"什么都不该关。
    #[test]
    fn there_is_nothing_to_close_past_either_end() {
        assert!(tabs_to_close(3, 0, CloseScope::Left).is_empty());
        assert!(tabs_to_close(3, 2, CloseScope::Right).is_empty());
    }

    /// 倒序是照着"从后往前删"给的。正着删的话，删完第 0 格之后
    /// 剩下的索引全错开一位，后面每一刀都砍在隔壁。
    #[test]
    fn the_doomed_tabs_come_back_back_to_front() {
        assert_eq!(tabs_to_close(4, 0, CloseScope::All), vec![3, 2, 1, 0]);
    }
}
