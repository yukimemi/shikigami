//! commit ごとの `jj diff`/`jj show` 出力と表示済み `Text` の二段キャッシュ。
//!
//! - Tier 1 (生データ):
//!   - [`DiffStore`]: commit ごとの `files` / `header_raw` / ファイル別生 diff
//!     (ディスク永続化 + バイト上限付き LRU)。
//!   - [`RawPatchCache`]: バッチ整形用の不変コミット全体 Git パッチ
//!     (`Arc<String>`, インメモリ・バイト上限付き LRU)。リサイズ時はここから
//!     再利用して `jj diff --git` を再実行しない。
//! - Tier 2 (表示結果):
//!   - [`RenderedDiffCache`]: ANSI パース済みの共有 [`ratatui::text::Text`]
//!     (`Arc<Text<'static>>`, インメモリ・バイト上限付き LRU)。バッチ整形結果と
//!     非同期フォールバック成功結果の両方を保持する。
//!   - [`HeaderCache`]: ANSI パース済みの change ヘッダ (`Arc<Text<'static>>`)。

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::text::{Line, Span, Text};
use serde::{Deserialize, Serialize};

use crate::jj::{DiffFile, Jj};

/// ディスクに残す commit の上限。
const CAP: usize = 300;
const MAX_DISK_BYTES: u64 = 64 * 1024 * 1024;

/// Tier 1 (`DiffStore`) のインメモリ生出力バイト上限 (32 MiB)。
const MAX_DIFF_STORE_BYTES: usize = 32 * 1024 * 1024;

/// Tier 1 (`RawPatchCache`) の生 Git パッチ合計バイト上限 (32 MiB)。
const MAX_RAW_PATCH_BYTES: usize = 32 * 1024 * 1024;

/// Tier 2 (`RenderedDiffCache`) の表示済み `Text` 合計バイト上限 (48 MiB)。
pub(crate) const MAX_RENDERED_BYTES: usize = 48 * 1024 * 1024;

/// Tier 2 (`RenderedDiffCache`) の最大エントリ数。
pub(crate) const MAX_RENDERED_ENTRIES: usize = 4096;

/// ヘッダ (`jj show --no-patch`) パース済み `Text` のキャッシュ上限。
const HEADER_CACHE_CAP: usize = 64;

/// `DiffFile` を表示名ではなく実パス + 変更種別で一意に識別するキー。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DiffFileKey {
    WholeCommit,
    File {
        status: char,
        source_path: String,
        destination_path: String,
    },
}

impl DiffFileKey {
    pub fn from_file(file: Option<&DiffFile>) -> Self {
        match file {
            Some(f) => Self::File {
                status: f.status,
                source_path: f.source_path.clone(),
                destination_path: f.destination_path.clone(),
            },
            None => Self::WholeCommit,
        }
    }

    pub fn raw_cache_path(&self) -> Option<String> {
        match self {
            Self::WholeCommit => None,
            Self::File {
                destination_path, ..
            } => Some(destination_path.clone()),
        }
    }
}

/// 表示キャッシュを整形経路ごとに分離するキー。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RenderModeKey {
    BatchDelta,
    Filter(String),
    Raw,
}

/// Tier 2 表示キャッシュのキー。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenderedKey {
    pub commit_id: String,
    pub file_key: DiffFileKey,
    pub width: u16,
    pub mode: RenderModeKey,
}

/// 1 commit 分の `jj` 生出力。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitCache {
    /// jj のファイル情報 (表示文字列と実パス)。
    pub files: Vec<DiffFile>,
    /// `jj show --no-patch --color always` の生出力 (ANSI 込み)。
    pub header_raw: String,
    /// 選択ファイルの実パス (`None` は change 全体) ごとの `jj diff --color
    /// always` 生出力。
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

    fn approx_bytes(&self) -> usize {
        let files_bytes: usize = self
            .files
            .iter()
            .map(|f| f.path.len() + f.source_path.len() + f.destination_path.len() + 32)
            .sum();
        let diffs_bytes: usize = self
            .diffs_raw
            .iter()
            .map(|(k, v)| k.as_ref().map_or(0, String::len) + v.len() + 32)
            .sum();
        files_bytes + self.header_raw.len() + diffs_bytes + 64
    }
}

/// `commit_id` → [`CommitCache`] の LRU。`load`/`save` でディスクの
/// 1 ファイルと同期する (repo ごとに 1 ファイル、詳細は [`cache_path`])。
#[derive(Debug)]
pub struct DiffStore {
    /// 使った順 (先頭が最も古い)。
    order: VecDeque<String>,
    map: HashMap<String, CommitCache>,
    total_bytes: usize,
    max_bytes: usize,
}

impl Default for DiffStore {
    fn default() -> Self {
        Self::with_max_bytes(MAX_DIFF_STORE_BYTES)
    }
}

impl DiffStore {
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            order: VecDeque::new(),
            map: HashMap::new(),
            total_bytes: 0,
            max_bytes,
        }
    }

    pub fn contains(&self, commit_id: &str) -> bool {
        self.map.contains_key(commit_id)
    }

    pub fn get(&self, commit_id: &str) -> Option<&CommitCache> {
        self.map.get(commit_id)
    }

    /// 新しい commit を入れる。件数またはバイト上限を超えたら最も古いものを消す。
    pub fn insert(&mut self, commit_id: String, entry: CommitCache) {
        let new_bytes = entry.approx_bytes();
        if new_bytes > self.max_bytes {
            return;
        }
        if let Some(prev) = self.map.remove(&commit_id) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.approx_bytes());
            self.forget_order(&commit_id);
        }
        self.order.push_back(commit_id.clone());
        self.total_bytes = self.total_bytes.saturating_add(new_bytes);
        self.map.insert(commit_id, entry);
        self.evict_excess();
    }

    /// 既存 commit にファイル別生 diff を 1 件追加する (バイト上限を維持)。
    pub fn insert_file_diff(&mut self, commit_id: &str, path: Option<String>, raw: String) {
        let Some(entry) = self.map.get_mut(commit_id) else {
            return;
        };
        let added_bytes = path.as_ref().map_or(0, String::len) + raw.len() + 32;
        if added_bytes > self.max_bytes {
            return;
        }
        let before = entry.approx_bytes();
        entry.diffs_raw.insert(path, raw);
        let after = entry.approx_bytes();
        if after >= before {
            self.total_bytes = self.total_bytes.saturating_add(after - before);
        } else {
            self.total_bytes = self.total_bytes.saturating_sub(before - after);
        }
        self.touch(commit_id);
        self.evict_excess();
    }

    /// 既存 commit を再訪した扱いにする (LRU の末尾へ移す)。
    pub fn touch(&mut self, commit_id: &str) {
        if self.map.contains_key(commit_id) {
            self.forget_order(commit_id);
            self.order.push_back(commit_id.to_string());
        }
    }

    #[cfg(test)]
    pub fn forget(&mut self, commit_id: &str) {
        self.forget_order(commit_id);
        if let Some(prev) = self.map.remove(commit_id) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.approx_bytes());
        }
    }

    fn evict_excess(&mut self) {
        while (self.order.len() > CAP || self.total_bytes > self.max_bytes) && self.order.len() > 1
        {
            if let Some(oldest) = self.order.pop_front()
                && let Some(removed) = self.map.remove(&oldest)
            {
                self.total_bytes = self.total_bytes.saturating_sub(removed.approx_bytes());
            }
        }
        if self.total_bytes > self.max_bytes
            && let Some(oldest) = self.order.pop_front()
            && let Some(removed) = self.map.remove(&oldest)
        {
            self.total_bytes = self.total_bytes.saturating_sub(removed.approx_bytes());
        }
    }

    fn forget_order(&mut self, commit_id: &str) {
        if let Some(pos) = self.order.iter().position(|id| id == commit_id) {
            self.order.remove(pos);
        }
    }

    /// repo ごとのキャッシュファイルから読み込む。無い/壊れていれば空。
    pub fn load(jj: &Jj) -> Self {
        cache_path(jj).map_or_else(Self::default, Self::load_file)
    }

    fn load_file(path: PathBuf) -> Self {
        let mut store = Self::default();
        let Ok(file) = std::fs::File::open(path) else {
            return store;
        };
        if file
            .metadata()
            .map_or(true, |meta| meta.len() > MAX_DISK_BYTES)
        {
            return store;
        }
        let mut bytes = Vec::new();
        if file
            .take(MAX_DISK_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > MAX_DISK_BYTES
        {
            return store;
        }
        let Ok(entries) = serde_json::from_slice::<Vec<(String, CommitCache)>>(&bytes) else {
            return store;
        };
        for (commit_id, entry) in entries {
            store.insert(commit_id, entry);
        }
        store
    }

    /// repo ごとのキャッシュファイルへ書き出す。ベストエフォート。
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
        if bytes.len() as u64 > MAX_DISK_BYTES {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, bytes);
    }
}

/// Tier 1: 不変コミット全体の生 Git パッチ (`jj diff --color never --git`) を
/// バイト上限付き LRU で保持する。リサイズ時はこれを再利用する。
#[derive(Debug)]
pub struct RawPatchCache {
    order: VecDeque<String>,
    map: HashMap<String, Arc<String>>,
    total_bytes: usize,
    max_bytes: usize,
}

impl Default for RawPatchCache {
    fn default() -> Self {
        Self::with_max_bytes(MAX_RAW_PATCH_BYTES)
    }
}

impl RawPatchCache {
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            order: VecDeque::new(),
            map: HashMap::new(),
            total_bytes: 0,
            max_bytes,
        }
    }

    pub fn get(&mut self, commit_id: &str) -> Option<Arc<String>> {
        let val = self.map.get(commit_id).cloned()?;
        self.touch(commit_id);
        Some(val)
    }

    pub fn insert(&mut self, commit_id: String, patch: Arc<String>) {
        let bytes = patch.len() + commit_id.len();
        if bytes > self.max_bytes {
            return;
        }
        if let Some(prev) = self.map.remove(&commit_id) {
            self.total_bytes = self
                .total_bytes
                .saturating_sub(prev.len() + commit_id.len());
            self.forget_order(&commit_id);
        }
        self.order.push_back(commit_id.clone());
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.map.insert(commit_id, patch);
        while self.total_bytes > self.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.map.remove(&oldest) {
                self.total_bytes = self
                    .total_bytes
                    .saturating_sub(removed.len() + oldest.len());
            }
        }
    }

    pub fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
        self.total_bytes = 0;
    }

    fn touch(&mut self, commit_id: &str) {
        self.forget_order(commit_id);
        self.order.push_back(commit_id.to_string());
    }

    fn forget_order(&mut self, commit_id: &str) {
        if let Some(pos) = self.order.iter().position(|id| id == commit_id) {
            self.order.remove(pos);
        }
    }
}

/// Tier 2: ANSI 変換済みの共有 `Text` をバイト上限・件数上限付き LRU で管理する。
#[derive(Debug)]
pub struct RenderedDiffCache {
    order: VecDeque<RenderedKey>,
    map: HashMap<RenderedKey, (Arc<Text<'static>>, usize)>,
    total_bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl Default for RenderedDiffCache {
    fn default() -> Self {
        Self::with_limits(MAX_RENDERED_BYTES, MAX_RENDERED_ENTRIES)
    }
}

impl RenderedDiffCache {
    pub fn with_limits(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            order: VecDeque::new(),
            map: HashMap::new(),
            total_bytes: 0,
            max_bytes,
            max_entries,
        }
    }

    pub fn get(&mut self, key: &RenderedKey) -> Option<Arc<Text<'static>>> {
        let val = self.map.get(key).map(|(text, _)| Arc::clone(text))?;
        self.forget_order(key);
        self.order.push_back(key.clone());
        Some(val)
    }

    pub fn insert(&mut self, key: RenderedKey, text: Arc<Text<'static>>) {
        let bytes = estimate_text_bytes(&text);
        if bytes > self.max_bytes || self.max_entries == 0 {
            return;
        }
        if let Some((_, prev_bytes)) = self.map.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(prev_bytes);
            self.forget_order(&key);
        }
        self.order.push_back(key.clone());
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.map.insert(key, (text, bytes));
        self.evict_excess();
    }

    /// バッチ全件をまとめて公開する。バッチ全体が上限を超える場合はキャッシュを
    /// 全消去しないよう、上限に収まる場合のみ登録する。
    pub fn insert_batch(&mut self, entries: Vec<(RenderedKey, Arc<Text<'static>>, usize)>) {
        if entries.is_empty() || entries.len() > self.max_entries {
            return;
        }
        let batch_bytes: usize = entries.iter().map(|(_, _, bytes)| *bytes).sum();
        if batch_bytes > self.max_bytes {
            return;
        }
        for (key, text, bytes) in entries {
            if let Some((_, prev_bytes)) = self.map.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(prev_bytes);
                self.forget_order(&key);
            }
            self.order.push_back(key.clone());
            self.total_bytes = self.total_bytes.saturating_add(bytes);
            self.map.insert(key, (text, bytes));
        }
        self.evict_excess();
    }

    pub fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
        self.total_bytes = 0;
    }

    #[cfg(test)]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn evict_excess(&mut self) {
        while self.total_bytes > self.max_bytes || self.map.len() > self.max_entries {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some((_, bytes)) = self.map.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(bytes);
            }
        }
    }

    fn forget_order(&mut self, key: &RenderedKey) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
    }
}

/// ANSI パース済みの change ヘッダを保持する小型 LRU。
#[derive(Debug, Default)]
pub struct HeaderCache {
    order: VecDeque<String>,
    map: HashMap<String, Arc<Text<'static>>>,
}

impl HeaderCache {
    pub fn get(&mut self, commit_id: &str) -> Option<Arc<Text<'static>>> {
        let val = self.map.get(commit_id).cloned()?;
        if let Some(pos) = self.order.iter().position(|id| id == commit_id) {
            self.order.remove(pos);
        }
        self.order.push_back(commit_id.to_string());
        Some(val)
    }

    pub fn insert(&mut self, commit_id: String, header: Arc<Text<'static>>) {
        if let Some(pos) = self.order.iter().position(|id| id == &commit_id) {
            self.order.remove(pos);
        }
        self.order.push_back(commit_id.clone());
        self.map.insert(commit_id, header);
        while self.order.len() > HEADER_CACHE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
    }
}

pub fn estimate_text_bytes(text: &Text<'static>) -> usize {
    let lines_bytes: usize = text
        .lines
        .iter()
        .map(|line| {
            let spans_bytes: usize = line
                .spans
                .iter()
                .map(|span| span.content.len() + std::mem::size_of::<Span<'static>>())
                .sum();
            spans_bytes + std::mem::size_of::<Line<'static>>()
        })
        .sum();
    lines_bytes.max(64)
}

/// OS ごとのキャッシュディレクトリ配下、repo root ごとに 1 ファイル。
/// `SHIKIGAMI_CACHE_DIR` があればそちらを優先する (テスト/カスタム用途)。
fn cache_path(jj: &Jj) -> Option<PathBuf> {
    let dir = match std::env::var_os("SHIKIGAMI_CACHE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => dirs::cache_dir()?.join("shikigami"),
    };
    let root = jj
        .root()
        .canonicalize()
        .unwrap_or_else(|_| jj.root().to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    // v3: diff 系コマンドの出力上限を適用したキャッシュだけを再利用する。
    Some(dir.join(format!("{:016x}-v3.json", hasher.finish())))
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
    fn raw_patch_and_rendered_caches_enforce_byte_budgets() {
        let mut raw_cache = RawPatchCache::with_max_bytes(200);
        raw_cache.insert("c1".into(), Arc::new("a".repeat(90)));
        raw_cache.insert("c2".into(), Arc::new("b".repeat(90)));
        assert!(raw_cache.get("c1").is_some());
        // c1 was touched, so inserting c3 evicts c2.
        raw_cache.insert("c3".into(), Arc::new("c".repeat(90)));
        assert!(raw_cache.get("c1").is_some());
        assert!(raw_cache.get("c2").is_none());
        assert!(raw_cache.get("c3").is_some());

        // Oversized single entry is ignored without wiping existing cache.
        raw_cache.insert("huge".into(), Arc::new("x".repeat(500)));
        assert!(raw_cache.get("huge").is_none());
        assert!(raw_cache.get("c1").is_some());

        let key = |name: &str| RenderedKey {
            commit_id: "c1".into(),
            file_key: DiffFileKey::File {
                status: 'M',
                source_path: name.into(),
                destination_path: name.into(),
            },
            width: 80,
            mode: RenderModeKey::BatchDelta,
        };
        let one_line = Arc::new(Text::from("x".repeat(120)));
        let one_bytes = estimate_text_bytes(&one_line);
        let mut rendered = RenderedDiffCache::with_limits(one_bytes * 2 + 10, 10);
        rendered.insert(key("a"), Arc::clone(&one_line));
        rendered.insert(key("b"), Arc::clone(&one_line));
        assert_eq!(rendered.len(), 2);
        // Touching "a" makes "b" oldest; inserting "c" evicts "b".
        assert!(rendered.get(&key("a")).is_some());
        rendered.insert(key("c"), Arc::clone(&one_line));
        assert!(rendered.get(&key("a")).is_some());
        assert!(rendered.get(&key("b")).is_none());
        assert!(rendered.get(&key("c")).is_some());
        assert!(rendered.total_bytes() <= one_bytes * 2 + 10);
    }

    #[test]
    fn legacy_display_only_cache_is_rejected_and_structured_paths_round_trip() {
        let legacy = r#"[["commit",{"files":[{"status":"R","path":"{literal => dir}/{a => b}"}],"header_raw":"","diffs_raw":[]}]]"#;
        assert!(serde_json::from_str::<Vec<(String, CommitCache)>>(legacy).is_err());

        let entry = CommitCache::new(
            vec![DiffFile {
                status: 'R',
                path: "{literal => dir}/{a => b}".to_owned(),
                source_path: "{literal => dir}/a".to_owned(),
                destination_path: "{literal => dir}/b".to_owned(),
            }],
            String::new(),
        );
        let bytes = serde_json::to_vec(&entry).unwrap();
        let restored: CommitCache = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.files, entry.files);
        assert_eq!(
            restored.files[0].diff_paths(),
            ["{literal => dir}/a", "{literal => dir}/b"]
        );
    }

    #[test]
    fn oversized_disk_cache_is_ignored_even_when_json_is_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.json");
        let entries = [("commit", CommitCache::new(Vec::new(), String::new()))];
        let json = serde_json::to_vec(&entries).unwrap();
        std::fs::write(&path, &json).unwrap();
        assert!(DiffStore::load_file(path.clone()).contains("commit"));

        let padding = MAX_DISK_BYTES + 1 - json.len() as u64;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::copy(&mut std::io::repeat(b' ').take(padding), &mut file).unwrap();
        drop(file);
        assert!(!DiffStore::load_file(path).contains("commit"));
    }

    #[test]
    fn save_then_load_round_trips_entries_and_lru_order() {
        let tmp = tempfile::tempdir().unwrap();
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
                source_path: "a.txt".to_string(),
                destination_path: "a.txt".to_string(),
            }],
            "header\n".to_string(),
        );
        entry
            .diffs_raw
            .insert(Some("a.txt".to_string()), "diff\n".to_string());
        entry
            .diffs_raw
            .insert(None, "whole change diff\n".to_string());
        store.insert("abc123".to_string(), entry);

        let mut legacy_hasher = std::collections::hash_map::DefaultHasher::new();
        jj.root().canonicalize().unwrap().hash(&mut legacy_hasher);
        let legacy_path = tmp
            .path()
            .join(format!("{:016x}.json", legacy_hasher.finish()));
        let legacy_entries = [("abc123", store.get("abc123").unwrap())];
        std::fs::write(legacy_path, serde_json::to_vec(&legacy_entries).unwrap()).unwrap();
        assert!(!DiffStore::load(&jj).contains("abc123"));
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
