//! 拖拽排序的落点计算。三个域（收藏、账户、桶）共用。

/// 把第 `from` 个元素移到第 `slot` 条缝上。
///
/// 缝的编号是 `0..=len`：0 是第一个元素前面那条，`len` 是最后一个元素后面那条。
/// 用缝而不是用"目标元素"来表达落点，是因为后者说不清"插在它前面还是后面"。
///
/// 往后挪时真实下标要减一：元素被拿走之后，排在它后面的都往前挪了一位。
pub fn move_to_slot<T>(items: &mut Vec<T>, from: usize, slot: usize) {
    if from >= items.len() || slot > items.len() {
        return;
    }
    // 落在自己两侧的缝上，位置没变
    if slot == from || slot == from + 1 {
        return;
    }
    let item = items.remove(from);
    let to = if slot > from { slot - 1 } else { slot };
    items.insert(to, item);
}

/// 把「显示列表里的第 `slot` 条缝」换算成「完整列表里的第几条缝」。
///
/// 桶可以藏起来，藏起来的不在显示列表里，两套编号因此对不上。落在显示列表第 `slot`
/// 条缝上，意思是「排在显示的第 `slot - 1` 个元素后面」——拿那个元素去完整列表里
/// 找位置，缝就在它后面一位。
pub fn slot_in_full(visible: &[String], full: &[String], slot: usize) -> usize {
    if slot == 0 {
        return 0;
    }
    let Some(anchor) = visible.get(slot - 1) else {
        return full.len();
    };
    full.iter()
        .position(|k| k == anchor)
        .map(|p| p + 1)
        .unwrap_or(full.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v() -> Vec<&'static str> {
        vec!["a", "b", "c"]
    }

    /// 缝的编号是 0..=len：0 在第一个元素前面，len 在最后一个后面。
    #[test]
    fn moves_forward_and_backward() {
        let mut items = v();
        move_to_slot(&mut items, 0, 2); // a 挪到 b 和 c 中间
        assert_eq!(items, vec!["b", "a", "c"]);

        let mut items = v();
        move_to_slot(&mut items, 0, 3); // a 挪到最后
        assert_eq!(items, vec!["b", "c", "a"]);

        let mut items = v();
        move_to_slot(&mut items, 2, 0); // c 挪到最前
        assert_eq!(items, vec!["c", "a", "b"]);
    }

    /// 落在自己两侧的缝上等于没动。不写这一条的话，
    /// "把 b 放到 b 后面"会变成先删后插，白存一次配置。
    #[test]
    fn dropping_next_to_itself_is_a_no_op() {
        let mut items = v();
        move_to_slot(&mut items, 1, 1);
        assert_eq!(items, vec!["a", "b", "c"]);

        let mut items = v();
        move_to_slot(&mut items, 1, 2);
        assert_eq!(items, vec!["a", "b", "c"]);
    }

    /// 越界不要 panic——拖拽过程中列表可能正好被后台刷新改过。
    #[test]
    fn out_of_range_is_ignored() {
        let mut items = v();
        move_to_slot(&mut items, 9, 0);
        assert_eq!(items, vec!["a", "b", "c"]);

        let mut items = v();
        move_to_slot(&mut items, 0, 9);
        assert_eq!(items, vec!["a", "b", "c"]);

        let mut empty: Vec<&str> = Vec::new();
        move_to_slot(&mut empty, 0, 0);
        assert!(empty.is_empty());
    }

    /// 藏起来的桶不在显示列表里，两套缝号对不上：显示列表里"b 后面"那条缝，
    /// 在完整列表里排第 3。不换算就会把桶插到错的地方。
    #[test]
    fn visible_slot_maps_into_the_full_list() {
        let full = ["a", "hidden", "b"].map(String::from).to_vec();
        let visible = ["a", "b"].map(String::from).to_vec();

        assert_eq!(slot_in_full(&visible, &full, 0), 0, "最前面还是最前面");
        assert_eq!(slot_in_full(&visible, &full, 1), 1, "a 后面");
        assert_eq!(
            slot_in_full(&visible, &full, 2),
            3,
            "b 后面，即完整列表末尾"
        );
    }

    /// 一个都没藏的时候，两套编号应该完全一致。
    #[test]
    fn slot_mapping_is_identity_without_hidden_items() {
        let full = ["a", "b"].map(String::from).to_vec();
        let visible = full.clone();
        for slot in 0..=2 {
            assert_eq!(slot_in_full(&visible, &full, slot), slot);
        }
    }
}
