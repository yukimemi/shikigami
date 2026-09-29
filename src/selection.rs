//! files pane の複数選択の状態と、それを各操作の対象へ落とす純関数。
//!
//! 端末にも jj にも依存しない — `app` が状態を持ち、`ui` が読むだけ。
//!
//! # 選択モデル
//!
//! - **mark**: `Space` で行ごとに付け外しする (飛び飛びの選択)。
//! - **visual 範囲**: `V` で anchor を置くと、anchor と cursor を両端とする
//!   閉区間が選択される。cursor は App の `file_selected` をそのまま使うので、
//!   j/k/g/G/Ctrl-d/u が範囲を自然に伸縮させる。
//! - 実効の対象集合は mark と visual 範囲の**和集合**。空なら操作は
//!   cursor の 1 ファイルに落ちる (選択が無いときの従来の挙動)。
//!
//! index ベースなので、`files` が入れ替わる箇所 (別 commit の表示・reload)
//! では呼び出し側が必ず [`FileSelection::clear`] する。
//!
//! # ディレクトリ行
//!
//! files pane は `jj diff --summary` のフラットなファイル一覧でディレクトリ行
//! を持たない。将来ディレクトリ行が加わっても、選択対象には含めない
//! (展開はしない) — 対象は常に実ファイルの行だけ。
//!
//! # jj に stage / unstage は無い
//!
//! jj には index が無いので stage/unstage/discard に相当する操作は無く、
//! files pane の「ファイル単位の操作」は restore (`r`) / absorb (`p`) /
//! open (`o`) の 3 つ。すべて [`FileSelection::targets`] を通る。

use std::collections::BTreeSet;

use crate::jj::DiffFile;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSelection {
    marks: BTreeSet<usize>,
    /// visual 範囲の起点。`Some` の間が visual mode。
    anchor: Option<usize>,
}

impl FileSelection {
    /// `i` の mark を反転する。
    pub fn toggle(&mut self, i: usize) {
        if !self.marks.remove(&i) {
            self.marks.insert(i);
        }
    }

    pub fn start_visual(&mut self, cursor: usize) {
        self.anchor = Some(cursor);
    }

    /// visual mode を抜ける。mark は残す。
    pub fn stop_visual(&mut self) {
        self.anchor = None;
    }

    pub fn is_visual(&self) -> bool {
        self.anchor.is_some()
    }

    pub fn is_marked(&self, i: usize) -> bool {
        self.marks.contains(&i)
    }

    /// `i` が visual 範囲 (anchor と `cursor` の閉区間) に入るか。
    pub fn in_range(&self, i: usize, cursor: usize) -> bool {
        match self.anchor {
            Some(a) => (a.min(cursor)..=a.max(cursor)).contains(&i),
            None => false,
        }
    }

    /// mark も visual も無い状態か。
    pub fn is_active(&self) -> bool {
        !self.marks.is_empty() || self.anchor.is_some()
    }

    pub fn clear(&mut self) {
        self.marks.clear();
        self.anchor = None;
    }

    /// mark ∪ visual 範囲。昇順・重複なしで、`len` 未満に切り詰める。
    /// 選択が無ければ空 (cursor へのフォールバックは [`Self::targets`])。
    pub fn selected(&self, cursor: usize, len: usize) -> Vec<usize> {
        let mut set: BTreeSet<usize> = self.marks.iter().copied().filter(|&i| i < len).collect();
        if let Some(a) = self.anchor {
            set.extend((a.min(cursor)..=a.max(cursor)).filter(|&i| i < len));
        }
        set.into_iter().collect()
    }

    /// 選択された行数 (和集合)。
    pub fn count(&self, cursor: usize, len: usize) -> usize {
        self.selected(cursor, len).len()
    }

    /// 操作の対象。選択が空なら cursor の 1 件、files が空なら空。
    pub fn targets(&self, cursor: usize, len: usize) -> Vec<usize> {
        let sel = self.selected(cursor, len);
        if !sel.is_empty() {
            sel
        } else if cursor < len {
            vec![cursor]
        } else {
            Vec::new()
        }
    }
}

/// バッチ操作に渡すパスと、対象外にした件数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchPlan {
    /// jj / editor に渡すパス (重複なし、対象順)。
    pub paths: Vec<String>,
    /// 実際に操作するファイル数。
    pub count: usize,
    /// この操作に不適で除外したファイル数。
    pub skipped: usize,
}

fn dedup_push(paths: &mut Vec<String>, path: String) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// restore: すべて有効。リネームは旧・新パスの両方を渡す。
pub fn plan_restore(files: &[DiffFile], targets: &[usize]) -> BatchPlan {
    let mut paths = Vec::new();
    let mut count = 0;
    for f in targets.iter().filter_map(|&i| files.get(i)) {
        count += 1;
        for p in f.restore_paths() {
            dedup_push(&mut paths, p);
        }
    }
    BatchPlan {
        paths,
        count,
        skipped: 0,
    }
}

/// absorb: リネーム/コピー (`R`/`C`) は jj が行を追えないので除外する。
pub fn plan_absorb(files: &[DiffFile], targets: &[usize]) -> BatchPlan {
    let mut paths = Vec::new();
    let mut count = 0;
    let mut skipped = 0;
    for f in targets.iter().filter_map(|&i| files.get(i)) {
        if matches!(f.status, 'R' | 'C') {
            skipped += 1;
        } else {
            count += 1;
            dedup_push(&mut paths, f.target_path());
        }
    }
    BatchPlan {
        paths,
        count,
        skipped,
    }
}

/// open: 単一対象は従来どおり何でも開く (削除済みでも)。複数対象のときだけ、
/// ディスクに無い `D` を除外する。
pub fn plan_open(files: &[DiffFile], targets: &[usize]) -> BatchPlan {
    let multi = targets.len() > 1;
    let mut paths = Vec::new();
    let mut count = 0;
    let mut skipped = 0;
    for f in targets.iter().filter_map(|&i| files.get(i)) {
        if multi && f.status == 'D' {
            skipped += 1;
        } else {
            count += 1;
            dedup_push(&mut paths, f.target_path());
        }
    }
    BatchPlan {
        paths,
        count,
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(status: char, path: &str) -> DiffFile {
        DiffFile {
            status,
            path: path.to_string(),
        }
    }

    #[test]
    fn toggle_flips_a_mark() {
        let mut s = FileSelection::default();
        s.toggle(2);
        assert!(s.is_marked(2) && s.is_active());
        s.toggle(2);
        assert!(!s.is_marked(2) && !s.is_active());
    }

    #[test]
    fn visual_range_follows_the_cursor_in_both_directions() {
        let mut s = FileSelection::default();
        s.start_visual(3);
        assert_eq!(s.selected(3, 10), vec![3]);
        assert_eq!(s.selected(5, 10), vec![3, 4, 5]);
        assert_eq!(s.selected(1, 10), vec![1, 2, 3]);
        assert!(s.in_range(2, 1) && !s.in_range(4, 1));
        s.stop_visual();
        assert!(!s.is_visual() && !s.is_active());
    }

    #[test]
    fn marks_and_range_union_is_sorted_and_deduplicated() {
        let mut s = FileSelection::default();
        s.toggle(0);
        s.toggle(3);
        s.start_visual(2);
        assert_eq!(s.selected(4, 10), vec![0, 2, 3, 4]);
        assert_eq!(s.count(4, 10), 4);
    }

    #[test]
    fn stop_visual_keeps_marks() {
        let mut s = FileSelection::default();
        s.toggle(1);
        s.start_visual(2);
        s.stop_visual();
        assert_eq!(s.selected(5, 10), vec![1]);
    }

    #[test]
    fn clear_drops_marks_and_range() {
        let mut s = FileSelection::default();
        s.toggle(1);
        s.start_visual(2);
        s.clear();
        assert!(!s.is_active());
        assert_eq!(s.selected(2, 10), Vec::<usize>::new());
    }

    #[test]
    fn targets_fall_back_to_the_cursor_and_clamp_to_len() {
        let mut s = FileSelection::default();
        assert_eq!(s.targets(2, 5), vec![2]);
        assert_eq!(s.targets(0, 0), Vec::<usize>::new());
        assert_eq!(s.count(2, 5), 0);
        s.toggle(1);
        s.toggle(9);
        assert_eq!(s.targets(2, 5), vec![1]);
        s.start_visual(3);
        assert_eq!(s.targets(8, 5), vec![1, 3, 4]);
    }

    #[test]
    fn restore_plan_expands_renames_and_dedups() {
        let files = [file('M', "a.rs"), file('R', "{x.rs => y.rs}")];
        let plan = plan_restore(&files, &[0, 1, 0]);
        assert_eq!(plan.paths, vec!["a.rs", "x.rs", "y.rs"]);
        assert_eq!((plan.count, plan.skipped), (3, 0));
    }

    #[test]
    fn absorb_plan_skips_renames_and_copies() {
        let files = [
            file('M', "a.rs"),
            file('R', "{x.rs => y.rs}"),
            file('C', "{p.rs => q.rs}"),
            file('A', "b.rs"),
        ];
        let plan = plan_absorb(&files, &[0, 1, 2, 3]);
        assert_eq!(plan.paths, vec!["a.rs", "b.rs"]);
        assert_eq!((plan.count, plan.skipped), (2, 2));

        let all_skipped = plan_absorb(&files, &[1, 2]);
        assert!(all_skipped.paths.is_empty());
        assert_eq!(all_skipped.skipped, 2);
    }

    #[test]
    fn open_plan_skips_deleted_files_only_for_multiple_targets() {
        let files = [
            file('M', "a.rs"),
            file('D', "gone.rs"),
            file('R', "{x => y}"),
        ];
        let multi = plan_open(&files, &[0, 1, 2]);
        assert_eq!(multi.paths, vec!["a.rs", "y"]);
        assert_eq!(multi.skipped, 1);
        let single = plan_open(&files, &[1]);
        assert_eq!(single.paths, vec!["gone.rs"]);
        assert_eq!(single.skipped, 0);
    }
}
