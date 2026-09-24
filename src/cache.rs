//! commit ごとの `jj diff`/`jj show` 出力をディスクへ永続化するキャッシュ。
//!
//! jj のオブジェクトストアは content-addressed: `commit_id` はコミットの
//! 内容 (親・木・メッセージ) から決まり、rewrite (describe/squash/rebase/
//! abandon/…) は既存の id を書き換えず新しい id を作る。だから
//! `commit_id` が同じである限り `jj diff`/`jj show` の出力はプロセスを
//! またいでも不変 — このキャッシュは commit の mutable/immutable を
//! 問わず、セッションをまたいでディスクに置いても安全 (jj 側で書き換え
//! られたら別の `commit_id` になるだけなので、古いエントリが誤って
//! 新しい内容として表示されることはない)。
//!
//! ANSI 解決後の [`ratatui::text::Text`] ではなく生の stdout 文字列で
//! 持つ: シリアライズ対象を素の `String`/`Vec` に留めておける (ratatui の
//! 内部表現に serde 対応を持ち込まずに済む) し、表示直前の
//! `ansi_to_tui` パースはプロセス起動と違って純 CPU 処理で軽い。

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::jj::{DiffFile, Jj};

/// ディスクに残す commit の上限。commit 1 件あたりのキャッシュはせいぜい
/// 数 KB 程度 (files + header + 数ファイル分の diff) なので、この件数
/// でも肥大化を気にする必要はない。超えた分は最も昔に使ったものから
/// 捨てる (LRU)。
const CAP: usize = 300;

/// 1 commit 分の `jj` 生出力。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitCache {
    /// `jj diff --summary` の結果。
    pub files: Vec<DiffFile>,
    /// `jj show --no-patch --color always` の生出力 (ANSI 込み)。
    pub header_raw: String,
    /// 選択ファイルパス (`None` は change 全体) ごとの `jj diff --color
    /// always` 生出力。
    ///
    /// JSON のオブジェクトキーは文字列でなければならず、serde_json は
    /// `HashMap` を素直にシリアライズしようとするとキー型に `Option<String>`
    /// (特に `None`) を許さずエラーにする。`diffs_raw_as_pairs` で
    /// `Vec<(Option<String>, String)>` へ変換してから載せる — 実行時の
    /// 参照 (`get`/`insert`/`contains_key`) は引き続き `HashMap` のまま。
    #[serde(with = "diffs_raw_as_pairs")]
    pub diffs_raw: HashMap<Option<String>, String>,
}

mod diffs_raw_as_pairs {
    use std::collections::HashMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        map: &HashMap<Option<String>, String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        map.iter()
            .collect::<Vec<(&Option<String>, &String)>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<Option<String>, String>, D::Error> {
        Ok(Vec::<(Option<String>, String)>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

impl CommitCache {
    pub fn new(files: Vec<DiffFile>, header_raw: String) -> Self {
        Self {
            files,
            header_raw,
            diffs_raw: HashMap::new(),
        }
    }
}

/// `commit_id` → [`CommitCache`] の LRU。`load`/`save` でディスクの
/// 1 ファイルと同期する (repo ごとに 1 ファイル、詳細は [`cache_path`])。
#[derive(Debug, Default)]
pub struct DiffStore {
    /// 使った順 (先頭が最も古い)。
    order: VecDeque<String>,
    map: HashMap<String, CommitCache>,
}

impl DiffStore {
    pub fn contains(&self, commit_id: &str) -> bool {
        self.map.contains_key(commit_id)
    }

    pub fn get(&self, commit_id: &str) -> Option<&CommitCache> {
        self.map.get(commit_id)
    }

    pub fn get_mut(&mut self, commit_id: &str) -> Option<&mut CommitCache> {
        self.map.get_mut(commit_id)
    }

    /// 新しい commit を入れる。上限を超えたら最も昔に使ったものを消す。
    pub fn insert(&mut self, commit_id: String, entry: CommitCache) {
        self.forget_order(&commit_id);
        self.order.push_back(commit_id.clone());
        self.map.insert(commit_id, entry);
        while self.order.len() > CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
    }

    /// 既存 commit を再訪した扱いにする (LRU の末尾へ移す)。
    pub fn touch(&mut self, commit_id: &str) {
        if self.map.contains_key(commit_id) {
            self.forget_order(commit_id);
            self.order.push_back(commit_id.to_string());
        }
    }

    /// テスト用: 1 commit ぶんのエントリを消す。起動直後の diff 先読み
    /// (`App::begin_diff_prefetch`) が対象を既に埋めてしまっているケースを
    /// 「まだキャッシュされていない」状態へ戻すのに使う。
    #[cfg(test)]
    pub fn forget(&mut self, commit_id: &str) {
        self.forget_order(commit_id);
        self.map.remove(commit_id);
    }

    fn forget_order(&mut self, commit_id: &str) {
        if let Some(pos) = self.order.iter().position(|id| id == commit_id) {
            self.order.remove(pos);
        }
    }

    /// repo ごとのキャッシュファイルから読み込む。無い/壊れていれば空。
    pub fn load(jj: &Jj) -> Self {
        let mut store = Self::default();
        let Some(path) = cache_path(jj) else {
            return store;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return store;
        };
        let Ok(entries) = serde_json::from_slice::<Vec<(String, CommitCache)>>(&bytes) else {
            return store;
        };
        // 保存時は古い順 (`order` の並びどおり) に書き出しているので、
        // そのまま insert すれば LRU の並びも復元される。
        for (commit_id, entry) in entries {
            store.insert(commit_id, entry);
        }
        store
    }

    /// repo ごとのキャッシュファイルへ書き出す。ベストエフォート:
    /// 書けなくても (権限・ディスク満杯・キャッシュ dir 不明など)
    /// 終了処理自体は失敗させない。
    pub fn save(&self, jj: &Jj) {
        let Some(path) = cache_path(jj) else {
            return;
        };
        let entries: Vec<(&String, &CommitCache)> = self
            .order
            .iter()
            .filter_map(|id| self.map.get(id).map(|entry| (id, entry)))
            .collect();
        let Ok(bytes) = serde_json::to_vec(&entries) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, bytes);
    }
}

/// OS ごとのキャッシュディレクトリ配下、repo root ごとに 1 ファイル。
/// `SHIKIGAMI_CACHE_DIR` があればそちらを優先する (テスト/カスタム用途)。
fn cache_path(jj: &Jj) -> Option<PathBuf> {
    let dir = match std::env::var_os("SHIKIGAMI_CACHE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::cache_dir()?.join("shikigami"),
    };
    // repo root ごとに別ファイルにする (commit_id は repo 内でしか一意性
    // を保証されない)。symlink 越し等で表記揺れしても同じキャッシュに
    // 当たるよう canonicalize してからハッシュ化する。
    let root = jj
        .root()
        .canonicalize()
        .unwrap_or_else(|_| jj.root().to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    Some(dir.join(format!("{:016x}.json", hasher.finish())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_evicts_the_least_recently_used_entry_past_capacity() {
        let mut store = DiffStore::default();
        for i in 0..CAP {
            store.insert(format!("c{i}"), CommitCache::new(Vec::new(), String::new()));
        }
        // c0 を touch して最近使った扱いにしてから、新しい commit を
        // 1 件足すと、touch していない次点の c1 が捨てられるはず。
        store.touch("c0");
        store.insert(
            "overflow".to_string(),
            CommitCache::new(Vec::new(), String::new()),
        );
        assert!(store.contains("c0"));
        assert!(!store.contains("c1"));
        assert!(store.contains("overflow"));
    }

    #[test]
    fn save_then_load_round_trips_entries_and_lru_order() {
        let tmp = tempfile::tempdir().unwrap();
        // SHIKIGAMI_CACHE_DIR で dirs::cache_dir() を経由せず temp dir を
        // 直接使わせ、テストが実行環境のホームディレクトリに書かないよう
        // にする。
        // SAFETY: テストは単一スレッドの `cargo test` ワーカー内で直列に
        // 環境変数を触っており、他のテストは `SHIKIGAMI_CACHE_DIR` を
        // 読まない。
        unsafe {
            std::env::set_var("SHIKIGAMI_CACHE_DIR", tmp.path());
        }
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".jj")).unwrap();
        let jj = Jj::discover(repo.path()).unwrap();

        let mut store = DiffStore::default();
        let mut entry = CommitCache::new(
            vec![DiffFile {
                status: 'M',
                path: "a.txt".to_string(),
            }],
            "header\n".to_string(),
        );
        entry
            .diffs_raw
            .insert(Some("a.txt".to_string()), "diff\n".to_string());
        // change 全体の diff (`None` キー) も一緒に持たせる: これは
        // JSON のオブジェクトキーとして素の `Option<String>` を渡すと
        // serde_json が拒否する組み合わせで、それを潰さず気付けるように
        // 混ぜておく。
        entry
            .diffs_raw
            .insert(None, "whole change diff\n".to_string());
        store.insert("abc123".to_string(), entry);
        store.save(&jj);

        let loaded = DiffStore::load(&jj);
        let restored = loaded.get("abc123").expect("entry persisted to disk");
        assert_eq!(restored.header_raw, "header\n");
        assert_eq!(
            restored.diffs_raw.get(&Some("a.txt".to_string())),
            Some(&"diff\n".to_string())
        );
        assert_eq!(
            restored.diffs_raw.get(&None),
            Some(&"whole change diff\n".to_string())
        );
        assert_eq!(restored.files[0].path, "a.txt");

        unsafe {
            std::env::remove_var("SHIKIGAMI_CACHE_DIR");
        }
    }
}
