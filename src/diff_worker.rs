//! diff 取得・整形・ANSI 変換をイベントループ外へ逃がす 2 レーンワーカー。
//!
//! - **メタデータワーカー**: `jj diff --template` (ファイル一覧) と
//!   `jj show --no-patch` (変更ヘッダ) を不変の完全 commit ID で取得し、
//!   ヘッダの ANSI 変換まで済ませて返す。
//! - **本文ワーカー**: `SHIKIGAMI_DIFF_BATCH=delta-v1` のバッチ整形または
//!   ファイル単位フォールバック / `SHIKIGAMI_DIFF_FILTER` を実行し、ANSI 変換
//!   済みの `Arc<Text<'static>>` を返す。
//!
//! 各ワーカーの未着手要求スロットは最新 1 件のみ (上書き集約) とし、実行中と
//! 同一の要求は重複投入しない。特にバッチ整形では同じ `(commit_id, width,
//! generation)` の実行中に files pane でファイル移動しても中断・重複起動せず、
//! 完了後にその時点の選択ファイルをキャッシュから即座に引く。

use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;

use anyhow::{Result, bail};
use ratatui::text::Text;

use crate::cache::{
    DiffFileKey, MAX_RENDERED_BYTES, MAX_RENDERED_ENTRIES, RawPatchCache, RenderModeKey,
    estimate_text_bytes,
};
use crate::diff_batch::{
    DeltaBatchConfig, FramedBatch, build_framed_batch, extract_and_validate_delta_output,
    parse_and_match_git_patch, run_delta_batch,
};
use crate::jj::{DiffFile, Jj};

/// バックグラウンドワーカーが利用する外部コマンド境界。
/// 本番は [`RealDiffBackend`]、テストでは決定論的に遅延・計数できる偽実装を差し込む。
pub trait DiffBackend: Send + Sync + 'static {
    fn diff_summary(&self, commit_id: &str) -> Result<Vec<DiffFile>>;
    fn show(&self, commit_id: &str) -> Result<String>;
    fn diff_formatter(&self) -> Result<String>;
    fn diff_git_full(&self, commit_id: &str) -> Result<String>;
    fn diff_file(&self, commit_id: &str, paths: &[String]) -> Result<String>;
    fn run_delta_batch(
        &self,
        config: &DeltaBatchConfig,
        framed: &FramedBatch,
        width: u16,
    ) -> Result<String>;
    fn run_diff_filter(&self, cmd: &str, raw: &str, width: u16) -> Result<String>;
}

#[derive(Debug, Clone)]
pub struct RealDiffBackend {
    jj: Jj,
}

impl RealDiffBackend {
    pub fn new(jj: Jj) -> Self {
        Self { jj }
    }
}

impl DiffBackend for RealDiffBackend {
    fn diff_summary(&self, commit_id: &str) -> Result<Vec<DiffFile>> {
        self.jj.diff_summary(commit_id)
    }

    fn show(&self, commit_id: &str) -> Result<String> {
        self.jj.show(commit_id)
    }

    fn diff_formatter(&self) -> Result<String> {
        self.jj.diff_formatter()
    }

    fn diff_git_full(&self, commit_id: &str) -> Result<String> {
        self.jj.diff_git_full(commit_id)
    }

    fn diff_file(&self, commit_id: &str, paths: &[String]) -> Result<String> {
        self.jj.diff(commit_id, paths)
    }

    fn run_delta_batch(
        &self,
        config: &DeltaBatchConfig,
        framed: &FramedBatch,
        width: u16,
    ) -> Result<String> {
        run_delta_batch(config, self.jj.root(), framed, width)
    }

    fn run_diff_filter(&self, cmd: &str, raw: &str, width: u16) -> Result<String> {
        crate::diff_filter::apply(cmd, raw, width)
    }
}

/// ANSI 付き文字列を ratatui の共有 `Text` へ変換する (ワーカー側で実行)。
fn parse_ansi(raw: &str) -> Arc<Text<'static>> {
    use ansi_to_tui::IntoText;
    let text = raw
        .into_text()
        .unwrap_or_else(|err| Text::from(format!("{err}")));
    Arc::new(text)
}

#[derive(Debug, Clone)]
pub struct MetaRequest {
    pub commit_id: String,
    pub generation: u64,
    pub cached: Option<(Arc<Vec<DiffFile>>, Arc<String>)>,
}

#[derive(Debug)]
pub enum MetaOutcome {
    Ready {
        files: Arc<Vec<DiffFile>>,
        /// 新規取得時だけ返す。キャッシュヒット時は既存エントリを置き換えない。
        header_raw: Option<Arc<String>>,
        header_text: Arc<Text<'static>>,
    },
    Error {
        files: Arc<Vec<DiffFile>>,
        header_text: Arc<Text<'static>>,
    },
}

#[derive(Debug)]
pub struct MetaResponse {
    pub commit_id: String,
    pub generation: u64,
    pub outcome: MetaOutcome,
}

#[derive(Debug, Clone)]
pub enum BodyStrategy {
    BatchDelta {
        config: DeltaBatchConfig,
        files: Arc<Vec<DiffFile>>,
        selected_file: Option<DiffFile>,
        cached_selected_raw_diff: Option<Arc<String>>,
    },
    PerFile {
        selected_file: Option<DiffFile>,
        filter_cmd: Option<String>,
        cached_raw_diff: Option<Arc<String>>,
    },
}

#[derive(Debug, Clone)]
pub struct BodyRequest {
    pub commit_id: String,
    pub width: u16,
    pub generation: u64,
    pub strategy: BodyStrategy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InFlightBody {
    Batch {
        commit_id: String,
        width: u16,
        generation: u64,
    },
    PerFile {
        commit_id: String,
        file_key: DiffFileKey,
        width: u16,
        mode: RenderModeKey,
        generation: u64,
    },
}

impl BodyRequest {
    fn in_flight_key(&self) -> InFlightBody {
        match &self.strategy {
            BodyStrategy::BatchDelta { .. } => InFlightBody::Batch {
                commit_id: self.commit_id.clone(),
                width: self.width,
                generation: self.generation,
            },
            BodyStrategy::PerFile {
                selected_file,
                filter_cmd,
                ..
            } => InFlightBody::PerFile {
                commit_id: self.commit_id.clone(),
                file_key: DiffFileKey::from_file(selected_file.as_ref()),
                width: if filter_cmd.is_some() { self.width } else { 0 },
                mode: match filter_cmd {
                    Some(cmd) => RenderModeKey::Filter(cmd.clone()),
                    None => RenderModeKey::Raw,
                },
                generation: self.generation,
            },
        }
    }
}

#[derive(Debug)]
pub struct SingleBodyPayload {
    pub file_key: DiffFileKey,
    pub mode: RenderModeKey,
    pub effective_width: u16,
    /// `jj diff` が今回新規に成功したときだけ `Some` (Tier 1 生キャッシュ登録用)。
    pub fetched_raw: Option<Arc<String>>,
    pub text: Arc<Text<'static>>,
    /// フィルタ失敗や `jj` エラーでない成功表示結果か (Tier 2 表示キャッシュ登録可否)。
    pub cacheable: bool,
    pub filter_error: Option<String>,
}

#[derive(Debug)]
pub struct BatchEntry {
    pub file_key: DiffFileKey,
    pub text: Arc<Text<'static>>,
    pub bytes: usize,
}

#[derive(Debug)]
pub enum BodyOutcome {
    BatchReady(Arc<[BatchEntry]>),
    /// `ui.diff-formatter` が `:git` ではないため、この世代全体でバッチを無効化し、
    /// 併せて現選択ファイルの従来経路結果を返す。
    BatchDisabledForGeneration(SingleBodyPayload),
    /// このコミット・幅のバッチを使えないため、現選択ファイルの従来経路結果を返す。
    BatchFallback {
        fallback: SingleBodyPayload,
    },
    SingleReady(SingleBodyPayload),
}

#[derive(Debug)]
pub struct BodyResponse {
    pub commit_id: String,
    pub width: u16,
    pub generation: u64,
    pub outcome: BodyOutcome,
}

struct MetaSlot {
    state: Mutex<MetaSlotState>,
    cv: Condvar,
}

struct MetaSlotState {
    pending: Option<MetaRequest>,
    in_flight: Option<(String, u64)>,
    shutdown: bool,
}

struct BodySlot {
    state: Mutex<BodySlotState>,
    cv: Condvar,
}

struct BodySlotState {
    pending: Option<BodyRequest>,
    in_flight: Option<InFlightBody>,
    shutdown: bool,
}

pub struct DiffWorkers {
    meta_slot: Arc<MetaSlot>,
    meta_rx: mpsc::Receiver<MetaResponse>,
    body_slot: Arc<BodySlot>,
    body_rx: mpsc::Receiver<BodyResponse>,
}

impl DiffWorkers {
    pub fn new(backend: Arc<dyn DiffBackend>) -> Self {
        let meta_slot = Arc::new(MetaSlot {
            state: Mutex::new(MetaSlotState {
                pending: None,
                in_flight: None,
                shutdown: false,
            }),
            cv: Condvar::new(),
        });
        let (meta_tx, meta_rx) = mpsc::channel();
        {
            let slot = Arc::clone(&meta_slot);
            let backend = Arc::clone(&backend);
            thread::spawn(move || meta_worker_loop(slot, backend, meta_tx));
        }

        let body_slot = Arc::new(BodySlot {
            state: Mutex::new(BodySlotState {
                pending: None,
                in_flight: None,
                shutdown: false,
            }),
            cv: Condvar::new(),
        });
        let (body_tx, body_rx) = mpsc::channel();
        {
            let slot = Arc::clone(&body_slot);
            thread::spawn(move || body_worker_loop(slot, backend, body_tx));
        }

        Self {
            meta_slot,
            meta_rx,
            body_slot,
            body_rx,
        }
    }

    pub fn submit_meta(&self, req: MetaRequest) {
        let mut guard = self.meta_slot.state.lock().unwrap();
        if guard
            .in_flight
            .as_ref()
            .is_some_and(|(cid, req_gen)| cid == &req.commit_id && *req_gen == req.generation)
        {
            guard.pending = None;
            return;
        }
        guard.pending = Some(req);
        self.meta_slot.cv.notify_one();
    }

    pub fn submit_body(&self, req: BodyRequest) {
        let key = req.in_flight_key();
        let mut guard = self.body_slot.state.lock().unwrap();
        if guard.in_flight.as_ref() == Some(&key) {
            guard.pending = None;
            return;
        }
        guard.pending = Some(req);
        self.body_slot.cv.notify_one();
    }

    pub fn cancel_pending_meta(&self) {
        let mut guard = self.meta_slot.state.lock().unwrap();
        guard.pending = None;
    }

    pub fn cancel_pending_body(&self) {
        let mut guard = self.body_slot.state.lock().unwrap();
        guard.pending = None;
    }

    pub fn cancel_all_pending(&self) {
        self.cancel_pending_meta();
        self.cancel_pending_body();
    }

    pub fn try_recv_meta(&self) -> Option<MetaResponse> {
        self.meta_rx.try_recv().ok()
    }

    pub fn try_recv_body(&self) -> Option<BodyResponse> {
        self.body_rx.try_recv().ok()
    }

    pub fn is_busy(&self) -> bool {
        let meta = self.meta_slot.state.lock().unwrap();
        if meta.pending.is_some() || meta.in_flight.is_some() {
            return true;
        }
        drop(meta);
        let body = self.body_slot.state.lock().unwrap();
        body.pending.is_some() || body.in_flight.is_some()
    }
}

impl Drop for DiffWorkers {
    fn drop(&mut self) {
        // 終了入力を待たせない: shutdown 通知だけ飛ばして join しない。
        {
            let mut guard = self.meta_slot.state.lock().unwrap();
            guard.pending = None;
            guard.shutdown = true;
            self.meta_slot.cv.notify_all();
        }
        {
            let mut guard = self.body_slot.state.lock().unwrap();
            guard.pending = None;
            guard.shutdown = true;
            self.body_slot.cv.notify_all();
        }
    }
}

fn meta_worker_loop(
    slot: Arc<MetaSlot>,
    backend: Arc<dyn DiffBackend>,
    tx: mpsc::Sender<MetaResponse>,
) {
    loop {
        let req = {
            let mut guard = slot.state.lock().unwrap();
            loop {
                if guard.shutdown {
                    return;
                }
                if let Some(req) = guard.pending.take() {
                    guard.in_flight = Some((req.commit_id.clone(), req.generation));
                    break req;
                }
                guard = slot.cv.wait(guard).unwrap();
            }
        };

        let outcome = if let Some((files, header_raw)) = req.cached {
            MetaOutcome::Ready {
                files,
                header_text: parse_ansi(&header_raw),
                header_raw: None,
            }
        } else {
            match (
                backend.diff_summary(&req.commit_id),
                backend.show(&req.commit_id),
            ) {
                (Ok(files), Ok(header_raw)) => {
                    let header_text = parse_ansi(&header_raw);
                    MetaOutcome::Ready {
                        files: Arc::new(files),
                        header_raw: Some(Arc::new(header_raw)),
                        header_text,
                    }
                }
                (files_res, header_res) => {
                    let files = Arc::new(files_res.unwrap_or_default());
                    let header_raw = header_res.unwrap_or_else(|err| format!("{err:#}"));
                    let header_text = parse_ansi(&header_raw);
                    MetaOutcome::Error { files, header_text }
                }
            }
        };

        let send_err = tx
            .send(MetaResponse {
                commit_id: req.commit_id,
                generation: req.generation,
                outcome,
            })
            .is_err();

        let mut guard = slot.state.lock().unwrap();
        guard.in_flight = None;
        if send_err || guard.shutdown {
            return;
        }
    }
}

fn body_worker_loop(
    slot: Arc<BodySlot>,
    backend: Arc<dyn DiffBackend>,
    tx: mpsc::Sender<BodyResponse>,
) {
    // 世代ごとの `ui.diff-formatter == ":git"` 判定キャッシュ。
    let mut formatter_check: Option<(u64, bool)> = None;
    let mut raw_patches = RawPatchCache::default();
    // UI に未配信の同一バッチも再整形しない。本文はレスポンスと共有する。
    let mut latest_batch: Option<(InFlightBody, Arc<[BatchEntry]>)> = None;

    loop {
        let req = {
            let mut guard = slot.state.lock().unwrap();
            loop {
                if guard.shutdown {
                    return;
                }
                if let Some(req) = guard.pending.take() {
                    guard.in_flight = Some(req.in_flight_key());
                    break req;
                }
                guard = slot.cv.wait(guard).unwrap();
            }
        };

        let outcome = match &req.strategy {
            BodyStrategy::BatchDelta {
                config,
                files,
                selected_file,
                cached_selected_raw_diff,
            } => {
                let is_git = match formatter_check {
                    Some((cached_gen, cached)) if cached_gen == req.generation => cached,
                    _ => {
                        let ok = backend
                            .diff_formatter()
                            .map(|f| f.trim() == ":git")
                            .unwrap_or(false);
                        formatter_check = Some((req.generation, ok));
                        ok
                    }
                };
                if !is_git {
                    let payload = execute_per_file(
                        &*backend,
                        &req.commit_id,
                        selected_file.as_ref(),
                        None,
                        req.width,
                        cached_selected_raw_diff.as_ref().map(|raw| raw.as_str()),
                    );
                    BodyOutcome::BatchDisabledForGeneration(payload)
                } else {
                    let key = req.in_flight_key();
                    let result = match &latest_batch {
                        Some((previous, entries)) if previous == &key => Ok(Arc::clone(entries)),
                        _ => prepare_batch(
                            &*backend,
                            config,
                            &req.commit_id,
                            files,
                            req.width,
                            &mut raw_patches,
                        )
                        .inspect(|entries| latest_batch = Some((key, Arc::clone(entries)))),
                    };
                    match result {
                        Ok(entries) => BodyOutcome::BatchReady(entries),
                        Err(_) => {
                            let fallback = execute_per_file(
                                &*backend,
                                &req.commit_id,
                                selected_file.as_ref(),
                                None,
                                req.width,
                                cached_selected_raw_diff.as_ref().map(|raw| raw.as_str()),
                            );
                            BodyOutcome::BatchFallback { fallback }
                        }
                    }
                }
            }
            BodyStrategy::PerFile {
                selected_file,
                filter_cmd,
                cached_raw_diff,
            } => {
                let payload = execute_per_file(
                    &*backend,
                    &req.commit_id,
                    selected_file.as_ref(),
                    filter_cmd.as_deref(),
                    req.width,
                    cached_raw_diff.as_ref().map(|raw| raw.as_str()),
                );
                BodyOutcome::SingleReady(payload)
            }
        };

        let send_err = tx
            .send(BodyResponse {
                commit_id: req.commit_id,
                width: req.width,
                generation: req.generation,
                outcome,
            })
            .is_err();

        let mut guard = slot.state.lock().unwrap();
        guard.in_flight = None;
        if send_err || guard.shutdown {
            return;
        }
    }
}

fn prepare_batch(
    backend: &dyn DiffBackend,
    config: &DeltaBatchConfig,
    commit_id: &str,
    files: &[DiffFile],
    width: u16,
    raw_patches: &mut RawPatchCache,
) -> Result<Arc<[BatchEntry]>> {
    if files.len() > MAX_RENDERED_ENTRIES {
        bail!("batch exceeds the rendered cache entry limit");
    }

    let cached = raw_patches.get(commit_id);
    let raw_patch = match &cached {
        Some(patch) => Arc::clone(patch),
        None => Arc::new(backend.diff_git_full(commit_id)?),
    };

    let matched = parse_and_match_git_patch(&raw_patch, files)?;
    if cached.is_none() {
        raw_patches.insert(commit_id.to_owned(), Arc::clone(&raw_patch));
    }
    let framed = build_framed_batch(&matched, &raw_patch, commit_id);
    let formatted = backend.run_delta_batch(config, &framed, width)?;
    let extracted = extract_and_validate_delta_output(&formatted, &framed, matched.len())?;

    let mut entries = Vec::with_capacity(matched.len());
    let mut total_bytes = 0;
    for (section, rendered) in matched.iter().zip(extracted.iter()) {
        let file = &files[section.file_index];
        let key = DiffFileKey::from_file(Some(file));
        let text = if rendered.trim().is_empty() {
            Arc::new(Text::from("(no changes)"))
        } else {
            parse_ansi(rendered)
        };
        let bytes = estimate_text_bytes(&text);
        total_bytes += bytes;
        if total_bytes > MAX_RENDERED_BYTES {
            bail!("batch exceeds the rendered cache byte limit");
        }
        entries.push(BatchEntry {
            file_key: key,
            bytes,
            text,
        });
    }
    Ok(entries.into())
}

fn execute_per_file(
    backend: &dyn DiffBackend,
    commit_id: &str,
    selected_file: Option<&DiffFile>,
    filter_cmd: Option<&str>,
    width: u16,
    cached_raw_diff: Option<&str>,
) -> SingleBodyPayload {
    let file_key = DiffFileKey::from_file(selected_file);
    let mode = match filter_cmd {
        Some(cmd) => RenderModeKey::Filter(cmd.to_string()),
        None => RenderModeKey::Raw,
    };
    let effective_width = if filter_cmd.is_some() { width } else { 0 };

    let mut fetched_raw = None;
    let diff_raw = match cached_raw_diff {
        Some(raw) => raw,
        None => {
            let paths = selected_file.map(DiffFile::diff_paths).unwrap_or_default();
            match backend.diff_file(commit_id, &paths) {
                Ok(raw) => fetched_raw.insert(Arc::new(raw)).as_str(),
                Err(err) => {
                    return SingleBodyPayload {
                        file_key,
                        mode,
                        effective_width,
                        fetched_raw: None,
                        text: parse_ansi(&format!("{err:#}")),
                        cacheable: false,
                        filter_error: None,
                    };
                }
            }
        }
    };

    if diff_raw.trim().is_empty() {
        return SingleBodyPayload {
            file_key,
            mode,
            effective_width,
            fetched_raw,
            text: Arc::new(Text::from("(no changes)")),
            cacheable: true,
            filter_error: None,
        };
    }

    match filter_cmd {
        Some(cmd) => match backend.run_diff_filter(cmd, diff_raw, width) {
            Ok(filtered) => SingleBodyPayload {
                file_key,
                mode,
                effective_width,
                fetched_raw,
                text: parse_ansi(&filtered),
                cacheable: true,
                filter_error: None,
            },
            Err(err) => SingleBodyPayload {
                file_key,
                mode,
                effective_width,
                text: parse_ansi(diff_raw),
                fetched_raw,
                cacheable: false,
                filter_error: Some(format!("{}: {err:#}", crate::diff_filter::DIFF_FILTER_ENV)),
            },
        },
        None => SingleBodyPayload {
            file_key,
            mode,
            effective_width,
            text: parse_ansi(diff_raw),
            fetched_raw,
            cacheable: true,
            filter_error: None,
        },
    }
}
