//! TUI の状態機械。
//!
//! 描画 (`ui`) と端末の面倒 (`tui`) からは独立させてあり、キー入力 →
//! 状態遷移 → jj 呼び出しまでをここで完結させる。おかげで、端末を開かずに
//! 「このキーで本当にこの jj コマンドが飛ぶか」をテストできる。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};

use crate::cache::{
    CommitCache, DiffFileKey, DiffStore, HeaderCache, RenderModeKey, RenderedDiffCache, RenderedKey,
};
use crate::diff_batch::DeltaBatchConfig;
use crate::diff_worker::{
    BodyOutcome, BodyRequest, BodyStrategy, DiffBackend, DiffWorkers, MetaOutcome, MetaRequest,
    RealDiffBackend, SingleBodyPayload, parse_ansi,
};
use crate::jj::{Change, DiffFile, Jj, RebaseMode, Row};
use crate::selection::{FileSelection, plan_absorb, plan_open, plan_restore, plan_untrack};

/// 一度に動かす行数 (Ctrl-d / Ctrl-u)。
const PAGE: usize = 10;

/// [`App::begin_reload`] が背景スレッドから返す `jj log`/`jj status` の
/// 結果。
type ReloadResult = Result<(Vec<Row>, String)>;

/// Ctrl-g で走らせた AI メッセージ生成 1 回分。
///
/// AI コマンド (と元になる `jj diff`) は数秒〜数十秒かかることがあり、
/// メインスレッドで待つと画面が固まって「押せたのか」が分からない。
/// 背景スレッドに逃がし、結果は [`App::poll_ai`] が描画ループから拾う。
/// その間 `started` からの経過でスピナーを回す (`ui::draw_input`)。
struct AiJob {
    rx: mpsc::Receiver<Result<String>>,
    started: Instant,
}

#[derive(Clone, Copy)]
enum RemoteAction {
    Fetch,
    Push,
    Done,
}

impl RemoteAction {
    fn label(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Push => "push",
            Self::Done => "done",
        }
    }
}

struct RemoteJob {
    action: RemoteAction,
    started: Instant,
    rx: mpsc::Receiver<(Result<String>, ReloadResult)>,
    quit_requested: bool,
}

fn status_detail(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// diff pane の本文状態。共有 [`Text`] で保持し、描画時は可視行だけを切り出す。
#[derive(Debug, Clone)]
pub enum DiffBody {
    Empty,
    LoadingMeta,
    LoadingDiff,
    Ready(Arc<Text<'static>>),
}

static LOADING_META_LINE: LazyLock<Line<'static>> = LazyLock::new(|| {
    Line::from(Span::styled(
        "loading…",
        Style::default().fg(Color::DarkGray),
    ))
});
static LOADING_DIFF_LINE: LazyLock<Line<'static>> = LazyLock::new(|| {
    Line::from(Span::styled(
        "loading diff…",
        Style::default().fg(Color::DarkGray),
    ))
});

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShownDiffKey {
    commit_id: String,
    file_key: DiffFileKey,
    width: u16,
    mode: RenderModeKey,
    generation: u64,
}

/// どちらの pane にキーが効くか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Log,
    /// 選択中 change のファイル一覧 (lazygit の files pane 相当)。
    Files,
    Diff,
}

/// pane の配置。`L` で循環する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneLayout {
    /// 現状のデフォルト: `log | (files - diff)`。log が全高、右カラムを
    /// files (上, 内容量に応じて伸縮) / diff (下) で縦に分ける。
    Stacked,
    /// `(log | files) - diff`。上段を log/files で横に分け、diff は
    /// 下段で全幅を使う — delta の side-by-side など、幅が要る diff
    /// フィルタ向け。
    DiffBelow,
    /// log / files / diff を全幅で縦に積む — 狭い端末向け。
    Vertical,
}

impl PaneLayout {
    pub fn label(self) -> &'static str {
        match self {
            PaneLayout::Stacked => "stacked",
            PaneLayout::DiffBelow => "diff-below",
            PaneLayout::Vertical => "vertical",
        }
    }

    pub fn next(self) -> Self {
        match self {
            PaneLayout::Stacked => PaneLayout::DiffBelow,
            PaneLayout::DiffBelow => PaneLayout::Vertical,
            PaneLayout::Vertical => PaneLayout::Stacked,
        }
    }
}

/// 入力待ちの用途。確定したときに何をするかを持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    /// `jj describe -r <rev> -m <input>`
    Describe { rev: String },
    /// `jj new <rev> -m <input>` (input 空なら `-m` なし)
    NewChild { rev: String },
    /// `jj commit -m <input>` — working copy (`@`) の describe + new を
    /// 1 操作で済ませる。`rev` は working copy の change id (AI 補完の元
    /// diff を選ぶのに使うだけで、コマンドには渡さない)。
    Commit { rev: String },
    /// `jj bookmark set <input> -r <rev>`
    Bookmark { rev: String },
    /// `jj rebase <mode> <rev> --onto <input>`
    RebaseOnto { rev: String, mode: RebaseMode },
    /// log pane の revset を差し替える
    Revset,
}

impl InputAction {
    /// AI 生成の元にする diff の対象 rev。bookmark 名や revset のように
    /// diff と無関係な入力では `None` (Ctrl-g を「使えない」と伝えるため
    /// 使う)。
    pub fn ai_rev(&self) -> Option<&str> {
        match self {
            InputAction::Describe { rev }
            | InputAction::NewChild { rev }
            | InputAction::Commit { rev } => Some(rev.as_str()),
            InputAction::Bookmark { .. } | InputAction::RebaseOnto { .. } | InputAction::Revset => {
                None
            }
        }
    }
}

/// 確認待ちの用途。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    Abandon {
        rev: String,
    },
    Squash {
        from: String,
        into: String,
    },
    Rebase {
        rev: String,
        onto: String,
        mode: RebaseMode,
    },
    /// `jj restore --changes-in <rev> <path>` — files pane でファイル 1 件
    /// の変更を破棄する。
    RestoreFile {
        rev: String,
        paths: Vec<String>,
        /// 対象ファイル数 (`paths` はリネームで 2 倍になりうる)。
        count: usize,
    },
    /// 必要なら `.gitignore` へ追記してから `jj file untrack` する。
    UntrackFile {
        ignore: bool,
        paths: Vec<String>,
        count: usize,
        skipped: usize,
    },
    Undo,
    Redo,
}

/// 1 行入力の状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub prompt: String,
    pub value: String,
    pub action: InputAction,
}

/// 確認ダイアログの状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub prompt: String,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Help,
    Input(Input),
    Confirm(Confirm),
}

/// status bar の見せ方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub text: String,
    pub kind: StatusKind,
}

impl Status {
    pub(crate) fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Info,
        }
    }

    pub(crate) fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Error,
        }
    }
}

/// files pane の選択ファイルを `$EDITOR`/`$VISUAL` で開くリクエスト。
/// 実際に子プロセスを起動して端末を明け渡すのは `tui::event_loop`
/// 側 (alternate screen / raw mode を握っているのはそちら) — ここでは
/// 「開きたい」という意思とパスだけを持たせて手渡す
/// ([`App::open_selected_file_in_editor`] / [`App::take_pending_editor`])。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorRequest {
    pub editor: String,
    pub paths: Vec<PathBuf>,
    /// 複数選択で対象外にした件数 (status 表示用)。
    pub skipped: usize,
}

pub struct App {
    pub jj: Jj,
    pub rows: Vec<Row>,
    /// `rows` の index。常に [`Row::Change`] を指す (connector は飛ばす)。
    pub selected: usize,
    pub revset: Option<String>,
    pub focus: Focus,
    pub mode: Mode,
    pub status: Status,
    /// squash / rebase の相手。`m` で置く。
    pub marked: Option<Change>,
    pub rebase_mode: RebaseMode,
    /// pane 配置。`L` でトグル (`ui::draw` 参照)。
    pub layout: PaneLayout,
    /// diff pane の本文 (共有済み ANSI 解決済み Text / 読み込み中状態)。
    pub diff_body: DiffBody,
    /// 選択中 change で変更されたファイル一覧 (lazygit の files pane 相当)。
    pub files: Vec<DiffFile>,
    /// `files` の index。ここで選んだ 1 ファイルだけが `diff` に出る。
    pub file_selected: usize,
    /// files pane の複数選択 (mark + visual 範囲)。`marked` (change の
    /// squash/rebase 相手) とは別物。
    pub file_selection: FileSelection,
    /// `jj show -r rev --no-patch` の結果 (共有済み ANSI 解決済み)。
    header: Arc<Text<'static>>,
    /// Tier 1: commit ごとの `jj diff --summary` / `jj show` / ファイル別生 diff
    /// のディスク永続 + バイト上限付きキャッシュ。キーは完全な `full_commit_id`。
    diff_cache: DiffStore,
    /// Tier 2: ANSI 変換済みの表示用 `Text` キャッシュ (バッチ / フォールバック共通)。
    rendered_cache: RenderedDiffCache,
    /// ANSI 変換済みの change ヘッダキャッシュ。
    header_cache: HeaderCache,
    /// メタデータ・本文の 2 レーン非同期 diff ワーカー。
    diff_workers: DiffWorkers,
    /// 再読込・変更世代。不一致の非同期結果・エラーはすべて棄却する。
    generation: u64,
    /// 直近に `files`/`header`/`file_selected` を反映した完全 commit_id。
    shown_commit: Option<String>,
    /// 現在メタデータ取得中の (完全 commit_id, 世代)。
    loading_meta_commit: Option<(String, u64)>,
    /// 直近に `diff_body` を確定反映した対象キー。
    shown_diff: Option<ShownDiffKey>,
    /// 現在本文取得中の対象キー。
    loading_diff_key: Option<ShownDiffKey>,
    /// 直近の `sync_diff` に渡された diff pane 幅。
    last_diff_width: Option<u16>,
    /// `ui.diff-formatter != :git` と判明してバッチを無効化した世代。
    batch_disabled_generation: Option<u64>,
    /// この世代でバッチが失敗/曖昧となりファイル単位フォールバックへ切り替えたコミット群。
    batch_fallback_commits: HashSet<String>,
    /// `status_fresh` により 1 フレームだけ表示を保留した diff-filter エラー。
    pending_filter_error: Option<(ShownDiffKey, String)>,
    /// 起動時にバックグラウンドで走らせた `jj log`/`jj status` の
    /// 結果を受け取るチャネル。終わるまでは `Some` で残る。
    startup_rx: Option<mpsc::Receiver<ReloadResult>>,
    /// 起動時の `jj log`/`jj status` が失敗したときのエラー。
    startup_error: Option<anyhow::Error>,
    /// 実行中の AI メッセージ生成 (Ctrl-g)。
    ai_job: Option<AiJob>,
    remote_job: Option<RemoteJob>,
    pub diff_scroll: u16,
    pub working_copy: String,
    /// `SHIKIGAMI_AI_CMD` の値。起動時に一度だけ読む (`App::new`)。
    pub ai_cmd: Option<String>,
    /// `SHIKIGAMI_DIFF_FILTER` の値。起動時に一度だけ読む (`App::new`)。
    pub diff_filter_cmd: Option<String>,
    /// `SHIKIGAMI_DIFF_BATCH=delta-v1` の設定。起動時に一度だけ読む (`App::new`)。
    pub diff_batch_config: Option<DeltaBatchConfig>,
    /// `EDITOR`/`VISUAL` の値。起動時に一度だけ読む (`App::new`)。
    pub editor_cmd: Option<String>,
    /// `o` (files pane) が立てた、まだ処理していないエディタ起動要求。
    pending_editor: Option<EditorRequest>,
    /// 直前の `handle_key` が `status` を実際に書き換えたか。
    status_fresh: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(jj: Jj, revset: Option<String>) -> Result<Self> {
        let backend = Arc::new(RealDiffBackend::new(jj.clone()));
        let mut app = Self::with_backend(jj, revset, backend, true);
        app.begin_reload();
        Ok(app)
    }

    pub(crate) fn with_backend(
        jj: Jj,
        revset: Option<String>,
        backend: Arc<dyn DiffBackend>,
        load_disk_cache: bool,
    ) -> Self {
        let diff_cache = if load_disk_cache {
            DiffStore::load(&jj)
        } else {
            DiffStore::default()
        };
        Self {
            jj,
            rows: Vec::new(),
            selected: 0,
            revset,
            focus: Focus::Log,
            mode: Mode::Normal,
            status: Status::info("loading…"),
            marked: None,
            rebase_mode: RebaseMode::Revision,
            layout: PaneLayout::Stacked,
            diff_body: DiffBody::Empty,
            files: Vec::new(),
            file_selected: 0,
            file_selection: FileSelection::default(),
            header: Arc::new(Text::default()),
            diff_cache,
            rendered_cache: RenderedDiffCache::default(),
            header_cache: HeaderCache::default(),
            diff_workers: DiffWorkers::new(backend),
            generation: 0,
            shown_commit: None,
            loading_meta_commit: None,
            shown_diff: None,
            loading_diff_key: None,
            last_diff_width: None,
            batch_disabled_generation: None,
            batch_fallback_commits: HashSet::new(),
            pending_filter_error: None,
            startup_rx: None,
            startup_error: None,
            ai_job: None,
            remote_job: None,
            diff_scroll: 0,
            working_copy: String::new(),
            ai_cmd: std::env::var(crate::ai::AI_CMD_ENV)
                .ok()
                .filter(|cmd| !cmd.trim().is_empty()),
            diff_filter_cmd: std::env::var(crate::diff_filter::DIFF_FILTER_ENV)
                .ok()
                .filter(|cmd| !cmd.trim().is_empty()),
            diff_batch_config: DeltaBatchConfig::from_env(),
            editor_cmd: crate::editor::resolve(),
            pending_editor: None,
            status_fresh: false,
            should_quit: false,
        }
    }

    /// 起動時の `jj log`/`jj status` を別スレッドで走らせる。
    ///
    /// Windows は `CreateProcess` が重く、これをメインスレッドで
    /// ブロッキングして待つと alternate screen に入るまで画面が固まって
    /// 見える。結果は [`Self::poll_startup`] が描画ループから
    /// ノンブロッキングで拾う。selection 変更時の `jj diff` (`sync_diff`)
    /// は対象と異なりキー入力のたびに起きるものではないので、そちらは
    /// 同期のまま — 非同期化するのはこの起動時 1 回きりの呼び出しだけ。
    fn begin_reload(&mut self) {
        let jj = self.jj.clone();
        let revset = self.revset.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = jj.log(revset.as_deref()).map(|rows| {
                let working_copy = jj.status_line().unwrap_or_default();
                (rows, working_copy)
            });
            let _ = tx.send(result);
        });
        self.startup_rx = Some(rx);
    }

    /// [`Self::begin_reload`] の結果をノンブロッキングで拾う。描画ループ
    /// から毎フレーム呼ぶ想定 (まだ終わっていなければ即座に戻る)。
    pub fn poll_startup(&mut self) {
        let Some(rx) = &self.startup_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((rows, working_copy))) => {
                self.apply_rows(rows, working_copy);
                self.startup_rx = None;
                if self.status.text == "loading…" {
                    self.status = Status::info("? for help");
                }
            }
            Ok(Err(err)) => {
                self.status = Status::error(format!("log failed: {err}"));
                self.startup_error = Some(err);
                self.should_quit = true;
                self.startup_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.startup_rx = None,
        }
    }

    /// 起動時 `jj log`/`jj status` の失敗を取り出す。`tui::run` が
    /// イベントループを抜けた後、これがあればプロセスの終了エラーに
    /// 変換する。
    pub fn take_startup_error(&mut self) -> Option<anyhow::Error> {
        self.startup_error.take()
    }

    /// 起動時のバックグラウンド `jj log` がまだ終わっていないか。
    /// 描画ループのポーリング間隔を詰めるのに使う。
    pub fn is_loading(&self) -> bool {
        self.startup_rx.is_some()
    }

    /// 非同期 diff ワーカーが進行中、または読み込み待ち状態にあるか。
    pub fn is_diff_loading(&self) -> bool {
        matches!(
            self.diff_body,
            DiffBody::LoadingMeta | DiffBody::LoadingDiff
        ) || self.diff_workers.is_busy()
    }

    /// 起動直後のテスト用: バックグラウンドの初回 `jj log` が終わるまで待つ。
    #[cfg(test)]
    pub fn wait_for_startup(&mut self) {
        while self.is_loading() {
            self.poll_startup();
            if self.is_loading() {
                thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// テスト用: 現在の選択に対する非同期 diff 取得・整形が終わるまで待つ。
    #[cfg(test)]
    pub fn sync_diff_blocking(&mut self, diff_width: u16) {
        let preserve_fresh = self.status_fresh;
        self.sync_diff(diff_width);
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.is_diff_loading() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for diff worker"
            );
            if preserve_fresh {
                self.status_fresh = true;
            }
            self.poll_diff();
            self.sync_diff(diff_width);
            if self.is_diff_loading() {
                thread::sleep(Duration::from_millis(1));
            }
        }
        self.status_fresh = false;
    }

    /// diff pane に表示する総行数 (変更ヘッダ + 本文)。
    pub fn diff_line_count(&self) -> usize {
        match &self.diff_body {
            DiffBody::Empty => self.header.lines.len(),
            DiffBody::LoadingMeta => 1,
            DiffBody::LoadingDiff => self.header.lines.len() + 1,
            DiffBody::Ready(body) => self.header.lines.len() + body.lines.len(),
        }
    }

    pub(crate) fn diff_lines(&self) -> impl Iterator<Item = &Line<'static>> {
        let (header, body) = match &self.diff_body {
            DiffBody::Empty => (self.header.lines.as_slice(), &[][..]),
            DiffBody::LoadingMeta => (&[][..], std::slice::from_ref(&*LOADING_META_LINE)),
            DiffBody::LoadingDiff => (
                self.header.lines.as_slice(),
                std::slice::from_ref(&*LOADING_DIFF_LINE),
            ),
            DiffBody::Ready(body) => (self.header.lines.as_slice(), body.lines.as_slice()),
        };
        header.iter().chain(body)
    }

    pub(crate) fn diff_draw_offset(&self) -> usize {
        match self.diff_body {
            DiffBody::LoadingMeta | DiffBody::LoadingDiff => 0,
            _ => self.diff_scroll.into(),
        }
    }

    #[cfg(test)]
    pub fn diff_plain_text(&self) -> String {
        self.diff_lines()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// AI メッセージ生成 (Ctrl-g) が走っている間の経過時間。走って
    /// いなければ `None`。描画側はこれでスピナーのコマと秒数を決める。
    pub fn ai_elapsed(&self) -> Option<Duration> {
        self.ai_job.as_ref().map(|job| job.started.elapsed())
    }

    /// [`Self::fill_with_ai`] が背景で走らせた生成の結果をノンブロッキング
    /// で拾い、入力値に反映する。描画ループから毎フレーム呼ぶ想定。
    pub fn poll_ai(&mut self) {
        let Some(job) = &self.ai_job else {
            return;
        };
        let result = match job.rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("generator thread died")),
        };
        self.ai_job = None;
        match result {
            // 生成中は Esc 以外のキーを受け付けない (`handle_input`) ので、
            // mode はまだ Ctrl-g を押した prompt のまま。
            Ok(message) => match &mut self.mode {
                Mode::Input(input) => {
                    input.value = message;
                    self.status = Status::info("ai: message generated");
                }
                _ => return,
            },
            Err(err) => self.status = Status::error(format!("ai: {err}")),
        }
        // `handle_key` と同じく、次の `sync_diff` に status を上書きさせない。
        self.status_fresh = true;
    }

    pub fn remote_progress(&self) -> Option<(&'static str, Duration)> {
        self.remote_job
            .as_ref()
            .map(|job| (job.action.label(), job.started.elapsed()))
    }

    pub fn remote_quit_requested(&self) -> bool {
        self.remote_job
            .as_ref()
            .is_some_and(|job| job.quit_requested)
    }

    fn begin_remote(&mut self, action: RemoteAction) {
        // A startup snapshot must never overwrite the post-operation log.
        self.startup_rx = None;
        self.diff_workers.cancel_all_pending();
        let jj = self.jj.clone();
        let revset = self.revset.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = match action {
                RemoteAction::Fetch => jj.git_fetch(),
                RemoteAction::Push => jj.git_push(),
                RemoteAction::Done => jj.bookmark_done(),
            };
            // Refresh even on failure: a remote operation may partially succeed.
            let rows = jj.log(revset.as_deref());
            let status = jj.status_line();
            let reload = rows.and_then(|rows| status.map(|status| (rows, status)));
            let _ = tx.send((result, reload));
        });
        self.remote_job = Some(RemoteJob {
            action,
            started: Instant::now(),
            rx,
            quit_requested: false,
        });
        self.status = Status::info(format!("{}: running…", action.label()));
    }

    pub fn poll_remote(&mut self) {
        let Some(job) = &self.remote_job else {
            return;
        };
        let (result, reload) = match job.rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => (
                Err(anyhow::anyhow!("remote worker thread died")),
                Err(anyhow::anyhow!("remote refresh unavailable")),
            ),
        };
        let job = self.remote_job.take().unwrap();
        let label = job.action.label();
        self.status = match result {
            Ok(output) => {
                let detail = status_detail(&output);
                Status::info(if detail.is_empty() {
                    format!("{label}: done")
                } else {
                    format!("{label}: {detail}")
                })
            }
            Err(err) => Status::error(format!(
                "{label} failed: {}",
                status_detail(&err.to_string())
            )),
        };
        match reload {
            Ok((rows, status)) => self.apply_rows(rows, status),
            Err(err) => {
                self.status = Status::error(format!(
                    "{} | reload failed: {}",
                    self.status.text,
                    status_detail(&err.to_string())
                ));
            }
        }
        self.status_fresh = true;
        self.should_quit |= job.quit_requested;
    }

    /// テスト用: AI 生成が終わるまで待って結果を反映する。
    #[cfg(test)]
    pub fn wait_for_ai(&mut self) {
        while self.ai_job.is_some() {
            self.poll_ai();
            if self.ai_job.is_some() {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// commit ごとの `jj` 出力キャッシュを、次回起動のためにディスクへ
    /// 書き出す。ベストエフォート ([`DiffStore::save`] 参照)。
    pub fn save_disk_cache(&self) {
        self.diff_cache.save(&self.jj);
    }

    /// log を読み直す。選択は change id で追いかけるので、squash や
    /// rebase で行数が変わってもカーソルが飛ばない。
    ///
    /// `Ctrl-r` と、jj を書き換えるコマンド成功後 (`report`) から呼ぶ
    /// 同期版。起動時だけは [`Self::begin_reload`] の非同期版を使う —
    /// こちらは「今すぐ結果が要る」呼び出し元しかいないので、非同期化
    /// する理由がない。
    pub fn reload(&mut self) -> Result<()> {
        // 起動時のバックグラウンド `jj log` がまだ走っている間に (revset
        // 変更や Ctrl-r で) ここが呼ばれることがある。この同期呼び出しは
        // 常に「今の revset での最新結果」を作るので、後から届く古い
        // startup 結果に上書きされないよう、その受信を捨てておく
        // (`poll_startup` は `startup_rx` が `None` なら何もしない)。
        self.startup_rx = None;
        let rows = self.jj.log(self.revset.as_deref())?;
        let working_copy = self.jj.status_line().unwrap_or_default();
        self.apply_rows(rows, working_copy);
        Ok(())
    }

    /// Preserve Diff's selection after the editor when the commit is unchanged.
    pub fn reload_after_editor(&mut self) -> Result<()> {
        let selection = self.file_selection.clone();
        let commit = self.selected_change().map(|c| c.commit_id.clone());
        self.reload()?;
        if self.focus == Focus::Diff
            && self.selected_change().map(|c| &c.commit_id) == commit.as_ref()
        {
            self.file_selection = selection;
        }
        Ok(())
    }

    /// `jj log`/`jj status` の結果を反映する。同期 (`reload`) / 非同期
    /// (`begin_reload` → `poll_startup`) の両方から呼ばれる共通処理。
    fn apply_rows(&mut self, rows: Vec<Row>, working_copy: String) {
        let keep = self.selected_change().map(|c| c.id.clone());
        self.file_selection.clear();
        self.rows = rows;
        self.selected = keep
            .and_then(|id| self.index_of(&id))
            .or_else(|| self.first_change_index())
            .unwrap_or(0);
        self.working_copy = working_copy;
        // 消えた change を mark したままにすると、以降の squash/rebase が
        // 毎回 jj 側のエラーになる。log から消えたら外す。
        if let Some(marked) = &self.marked
            && self.index_of(&marked.id).is_none()
        {
            self.marked = None;
        }
        // 世代を進めて再読込前の未着手要求を破棄し、遅着結果を拒否する。
        self.generation = self.generation.wrapping_add(1);
        self.diff_workers.cancel_all_pending();
        self.pending_filter_error = None;
        self.loading_meta_commit = None;
        self.loading_diff_key = None;
        self.shown_diff = None;
        let current_commit = self.selected_change().map(|c| c.full_commit_id.as_str());
        if self.shown_commit.as_deref() != current_commit {
            self.files.clear();
            self.file_selected = 0;
            self.header = Arc::new(Text::default());
            self.diff_body = DiffBody::Empty;
            self.shown_commit = None;
            self.diff_scroll = 0;
        }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.rows.iter().position(|row| match row {
            Row::Change { change, .. } => change.id == id,
            Row::Connector(_) => false,
        })
    }

    fn first_change_index(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Change { .. }))
    }

    pub fn selected_change(&self) -> Option<&Change> {
        match self.rows.get(self.selected) {
            Some(Row::Change { change, .. }) => Some(change),
            _ => None,
        }
    }

    /// selection を `delta` 行動かす。connector 行は跨ぐ。
    pub fn move_selection(&mut self, delta: isize) {
        let changes: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, Row::Change { .. }))
            .map(|(i, _)| i)
            .collect();
        if changes.is_empty() {
            return;
        }
        let cur = changes
            .iter()
            .position(|&i| i == self.selected)
            .unwrap_or(0);
        let next = (cur as isize + delta).clamp(0, changes.len() as isize - 1) as usize;
        self.selected = changes[next];
    }

    pub fn select_first(&mut self) {
        if let Some(i) = self.first_change_index() {
            self.selected = i;
        }
    }

    pub fn select_last(&mut self) {
        if let Some(i) = self
            .rows
            .iter()
            .rposition(|row| matches!(row, Row::Change { .. }))
        {
            self.selected = i;
        }
    }

    /// 選択中 change のファイル一覧の 1 件 (files pane で選ばれているもの)。
    pub fn selected_file(&self) -> Option<&DiffFile> {
        self.files.get(self.file_selected)
    }

    /// files pane の選択を `delta` 行動かす。
    fn move_file_selection(&mut self, delta: isize) {
        if self.files.is_empty() {
            return;
        }
        let next = (self.file_selected as isize + delta).clamp(0, self.files.len() as isize - 1);
        self.file_selected = next as usize;
    }

    fn select_first_file(&mut self) {
        self.file_selected = 0;
    }

    fn select_last_file(&mut self) {
        self.file_selected = self.files.len().saturating_sub(1);
    }

    fn can_batch_commit(&self, commit_id: &str) -> bool {
        self.diff_filter_cmd.is_none()
            && self.diff_batch_config.is_some()
            && self.batch_disabled_generation != Some(self.generation)
            && !self.batch_fallback_commits.contains(commit_id)
            && !self.files.is_empty()
    }

    fn active_render_mode(&self, commit_id: &str, diff_width: u16) -> (RenderModeKey, u16) {
        if let Some(cmd) = &self.diff_filter_cmd {
            (RenderModeKey::Filter(cmd.clone()), diff_width)
        } else if self.can_batch_commit(commit_id) {
            (RenderModeKey::BatchDelta, diff_width)
        } else {
            (RenderModeKey::Raw, 0)
        }
    }

    /// バックグラウンド diff ワーカーの完了結果をノンブロッキングで反映する。
    pub fn poll_diff(&mut self) {
        while let Some(resp) = self.diff_workers.try_recv_meta() {
            if resp.generation != self.generation {
                continue;
            }
            let is_current = self
                .selected_change()
                .is_some_and(|c| c.full_commit_id == resp.commit_id);
            match resp.outcome {
                MetaOutcome::Ready {
                    files,
                    header_raw,
                    header_text,
                } => {
                    self.diff_cache.insert(
                        resp.commit_id.clone(),
                        CommitCache::new(files.clone(), header_raw),
                    );
                    self.header_cache
                        .insert(resp.commit_id.clone(), Arc::clone(&header_text));
                    if is_current {
                        self.files = files;
                        self.header = header_text;
                        self.file_selected = 0;
                        self.file_selection.clear();
                        self.shown_commit = Some(resp.commit_id);
                        self.loading_meta_commit = None;
                        self.shown_diff = None;
                        self.loading_diff_key = None;
                        self.diff_body = DiffBody::LoadingDiff;
                        self.diff_scroll = 0;
                    }
                }
                MetaOutcome::Error { files, header_text } => {
                    // 失敗はキャッシュしない。現在選択中の commit だけ表示に反映する。
                    if is_current {
                        self.files = files;
                        self.header = header_text;
                        self.file_selected = 0;
                        self.file_selection.clear();
                        self.shown_commit = Some(resp.commit_id.clone());
                        self.loading_meta_commit = None;
                        self.loading_diff_key = None;
                        self.diff_body = DiffBody::Empty;
                        self.diff_scroll = 0;
                        if self.files.is_empty() {
                            let width = self.last_diff_width.unwrap_or(80);
                            let (mode, effective_width) =
                                self.active_render_mode(&resp.commit_id, width);
                            self.shown_diff = Some(ShownDiffKey {
                                commit_id: resp.commit_id,
                                file_key: DiffFileKey::WholeCommit,
                                width: effective_width,
                                mode,
                                generation: resp.generation,
                            });
                        } else {
                            self.shown_diff = None;
                        }
                    }
                }
            }
        }

        while let Some(resp) = self.diff_workers.try_recv_body() {
            if resp.generation != self.generation {
                continue;
            }
            match resp.outcome {
                BodyOutcome::BatchReady(entries) => {
                    let mut batch_to_cache = Vec::with_capacity(entries.len());
                    let mut current_match: Option<Arc<Text<'static>>> = None;
                    let is_current_commit = self
                        .selected_change()
                        .is_some_and(|c| c.full_commit_id == resp.commit_id)
                        && self.shown_commit.as_deref() == Some(resp.commit_id.as_str())
                        && self.last_diff_width == Some(resp.width)
                        && self.diff_filter_cmd.is_none();
                    let current_file_key =
                        is_current_commit.then(|| DiffFileKey::from_file(self.selected_file()));

                    for entry in entries.iter() {
                        if current_file_key.as_ref() == Some(&entry.file_key) {
                            current_match = Some(Arc::clone(&entry.text));
                        }
                        batch_to_cache.push((
                            RenderedKey {
                                commit_id: resp.commit_id.clone(),
                                file_key: entry.file_key.clone(),
                                width: resp.width,
                                mode: RenderModeKey::BatchDelta,
                            },
                            Arc::clone(&entry.text),
                            entry.bytes,
                        ));
                    }
                    self.rendered_cache.insert_batch(batch_to_cache);

                    if let (Some(file_key), Some(text)) = (current_file_key, current_match) {
                        self.diff_body = DiffBody::Ready(text);
                        self.shown_diff = Some(ShownDiffKey {
                            commit_id: resp.commit_id,
                            file_key,
                            width: resp.width,
                            mode: RenderModeKey::BatchDelta,
                            generation: resp.generation,
                        });
                        self.loading_diff_key = None;
                        self.pending_filter_error = None;
                    }
                }
                BodyOutcome::BatchDisabledForGeneration(payload) => {
                    self.batch_disabled_generation = Some(resp.generation);
                    self.apply_single_body(resp.commit_id, resp.generation, payload);
                }
                BodyOutcome::BatchFallback { fallback } => {
                    self.batch_fallback_commits.insert(resp.commit_id.clone());
                    self.apply_single_body(resp.commit_id, resp.generation, fallback);
                }
                BodyOutcome::SingleReady(payload) => {
                    self.apply_single_body(resp.commit_id, resp.generation, payload);
                }
            }
        }
    }

    fn apply_single_body(
        &mut self,
        commit_id: String,
        generation: u64,
        payload: SingleBodyPayload,
    ) {
        if let Some(raw) = payload.fetched_raw {
            self.diff_cache
                .insert_file_diff(&commit_id, payload.file_key.raw_cache_path(), raw);
        }
        if payload.cacheable {
            self.rendered_cache.insert(
                RenderedKey {
                    commit_id: commit_id.clone(),
                    file_key: payload.file_key.clone(),
                    width: payload.effective_width,
                    mode: payload.mode.clone(),
                },
                Arc::clone(&payload.text),
            );
        }

        let Some(current) = self.selected_change() else {
            return;
        };
        if current.full_commit_id != commit_id
            || self.shown_commit.as_deref() != Some(commit_id.as_str())
        {
            return;
        }
        let current_file_key = DiffFileKey::from_file(self.selected_file());
        if current_file_key != payload.file_key {
            return;
        }
        let current_width = self.last_diff_width.unwrap_or(80);
        let (expected_mode, expected_width) = self.active_render_mode(&commit_id, current_width);
        if expected_mode != payload.mode || expected_width != payload.effective_width {
            return;
        }

        let key = ShownDiffKey {
            commit_id,
            file_key: payload.file_key,
            width: payload.effective_width,
            mode: payload.mode,
            generation,
        };
        self.diff_body = DiffBody::Ready(payload.text);
        self.loading_diff_key = None;
        if let Some(err_msg) = payload.filter_error {
            if self.status_fresh {
                self.pending_filter_error = Some((key.clone(), err_msg));
            } else {
                self.status = Status::error(err_msg);
                self.pending_filter_error = None;
            }
        } else {
            self.pending_filter_error = None;
        }
        self.shown_diff = Some(key);
    }

    /// 選択中 commit / ファイルと表示幅に合わせて diff 状態を非同期同期する。
    ///
    /// イベントループからは一切サブプロセス待ちも巨大パッチの ANSI 変換も行わない。
    /// キャッシュ命中なら即座に共有 `Text` を差し替え、未命中なら表示を「待機中」へ
    /// 切り替えてバックグラウンドワーカーへ最新要求だけを送る。
    pub fn sync_diff(&mut self, diff_width: u16) {
        if self.remote_job.is_some() {
            return;
        }
        self.last_diff_width = Some(diff_width);
        self.poll_diff();
        let status_just_reported = std::mem::take(&mut self.status_fresh);

        let Some(change) = self.selected_change() else {
            self.files.clear();
            self.file_selected = 0;
            self.file_selection.clear();
            self.header = Arc::new(Text::default());
            self.diff_body = DiffBody::Empty;
            self.shown_commit = None;
            self.loading_meta_commit = None;
            self.shown_diff = None;
            self.loading_diff_key = None;
            self.pending_filter_error = None;
            self.diff_workers.cancel_all_pending();
            return;
        };
        let commit_id = change.full_commit_id.clone();

        // 1. コミットメタデータ (files + header) の同期。
        if self.shown_commit.as_deref() != Some(commit_id.as_str()) {
            if self.diff_cache.contains(&commit_id) {
                self.diff_cache.touch(&commit_id);
                let entry = self
                    .diff_cache
                    .get(&commit_id)
                    .expect("checked contains above");
                let files = entry.files.clone();
                let header = match self.header_cache.get(&commit_id) {
                    Some(h) => h,
                    None => {
                        let parsed = parse_ansi(&entry.header_raw);
                        self.header_cache
                            .insert(commit_id.clone(), Arc::clone(&parsed));
                        parsed
                    }
                };
                self.files = files;
                self.header = header;
                self.file_selected = 0;
                self.file_selection.clear();
                self.shown_commit = Some(commit_id.clone());
                self.loading_meta_commit = None;
                self.shown_diff = None;
                self.loading_diff_key = None;
                self.pending_filter_error = None;
                self.diff_scroll = 0;
                self.diff_workers.cancel_pending_meta();
            } else {
                // 未取得: 旧コミットの files / header / diff を残さず即座に待機表示へ。
                let already_loading = self
                    .loading_meta_commit
                    .as_ref()
                    .is_some_and(|(cid, g)| cid == &commit_id && *g == self.generation);
                if !already_loading {
                    self.files.clear();
                    self.file_selected = 0;
                    self.file_selection.clear();
                    self.header = Arc::new(Text::default());
                    self.diff_body = DiffBody::LoadingMeta;
                    self.shown_commit = None;
                    self.shown_diff = None;
                    self.loading_diff_key = None;
                    self.pending_filter_error = None;
                    self.diff_scroll = 0;
                    self.loading_meta_commit = Some((commit_id.clone(), self.generation));
                }
                self.diff_workers.cancel_pending_body();
                self.diff_workers.submit_meta(MetaRequest {
                    commit_id,
                    generation: self.generation,
                });
                return;
            }
        }

        // 2. 本文 (選択中ファイル + 表示幅 + 整形経路) の同期。
        let file_key = DiffFileKey::from_file(self.selected_file());
        let (mode, effective_width) = self.active_render_mode(&commit_id, diff_width);
        let target_key = ShownDiffKey {
            commit_id: commit_id.clone(),
            file_key: file_key.clone(),
            width: effective_width,
            mode: mode.clone(),
            generation: self.generation,
        };

        if self.shown_diff.as_ref() == Some(&target_key) {
            if !status_just_reported
                && let Some((err_key, err_msg)) = self.pending_filter_error.take()
                && err_key == target_key
            {
                self.status = Status::error(err_msg);
            }
            return;
        }

        // 選択対象 (commit または file) が変わったときだけ scroll を先頭へ戻す。
        let prev_selection = self
            .shown_diff
            .as_ref()
            .or(self.loading_diff_key.as_ref())
            .map(|k| (&k.commit_id, &k.file_key));
        let selection_changed = prev_selection != Some((&commit_id, &file_key));
        if selection_changed {
            self.diff_scroll = 0;
        }

        let rendered_key = RenderedKey {
            commit_id: commit_id.clone(),
            file_key: file_key.clone(),
            width: effective_width,
            mode: mode.clone(),
        };
        if let Some(cached_text) = self.rendered_cache.get(&rendered_key) {
            self.diff_body = DiffBody::Ready(cached_text);
            self.shown_diff = Some(target_key);
            self.loading_diff_key = None;
            self.pending_filter_error = None;
            self.diff_workers.cancel_pending_body();
            return;
        }

        // 未命中: 前のファイルの diff を現ファイルのタイトル下に残さない。
        self.diff_body = DiffBody::LoadingDiff;
        self.shown_diff = None;
        self.loading_diff_key = Some(target_key);
        self.pending_filter_error = None;

        let cached_raw_file_diff = self
            .diff_cache
            .get(&commit_id)
            .and_then(|entry| entry.diffs_raw.get(&file_key.raw_cache_path()))
            .cloned();

        let strategy = match mode {
            RenderModeKey::BatchDelta => {
                let config = self
                    .diff_batch_config
                    .clone()
                    .expect("BatchDelta mode requires diff_batch_config");
                BodyStrategy::BatchDelta {
                    config,
                    files: self.files.clone(),
                    selected_file: self.selected_file().cloned(),
                    cached_selected_raw_diff: cached_raw_file_diff,
                }
            }
            RenderModeKey::Filter(cmd) => BodyStrategy::PerFile {
                selected_file: self.selected_file().cloned(),
                filter_cmd: Some(cmd),
                cached_raw_diff: cached_raw_file_diff,
            },
            RenderModeKey::Raw => BodyStrategy::PerFile {
                selected_file: self.selected_file().cloned(),
                filter_cmd: None,
                cached_raw_diff: cached_raw_file_diff,
            },
        };

        self.diff_workers.submit_body(BodyRequest {
            commit_id,
            width: diff_width,
            generation: self.generation,
            strategy,
        });
    }

    pub fn scroll_diff(&mut self, delta: isize) {
        let next = self.diff_scroll as isize + delta;
        let max = self.diff_line_count().saturating_sub(1) as isize;
        self.diff_scroll = next.clamp(0, max.max(0)) as u16;
    }

    /// jj を呼んだ結果を status bar に載せる。
    ///
    /// 成功時は jj の stdout の 1 行目 (`Working copy (@) now at: ...`) を
    /// 出す。ユーザにとって「何が起きたか」はそこに全部書いてある。
    fn report(&mut self, label: &str, result: Result<String>) {
        match result {
            Ok(out) => {
                let detail = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                self.status = Status::info(if detail.is_empty() {
                    label.to_string()
                } else {
                    format!("{label}: {detail}")
                });
                if let Err(err) = self.reload() {
                    self.status = Status::error(format!("reload failed: {err}"));
                }
            }
            Err(err) => {
                // jj のエラーは複数行 (Error/Caused by/Hint)。status bar は
                // 1 行なので、改行を ` | ` に畳んで全文を残す。
                let text = status_detail(&err.to_string());
                self.status = Status::error(format!("{label} failed: {text}"));
            }
        }
    }

    /// [`Self::report`] のバッチ版。複数ファイルなら件数をラベルに入れ、
    /// 対象外にした件数を末尾に足す。
    fn report_batch(&mut self, label: &str, count: usize, skipped: usize, result: Result<String>) {
        let label = if count > 1 {
            format!("{label} {count} files")
        } else {
            label.to_string()
        };
        self.report(&label, result);
        if skipped > 0 {
            self.status.text.push_str(&format!(" ({skipped} skipped)"));
        }
    }

    /// 確定した入力を実行する。
    pub fn submit_input(&mut self, input: Input) {
        match input.action {
            InputAction::Describe { rev } => {
                let result = self.jj.describe(&rev, &input.value);
                self.report("describe", result);
            }
            InputAction::NewChild { rev } => {
                let message = (!input.value.is_empty()).then_some(input.value.as_str());
                let result = self.jj.new_child(&rev, message);
                self.report("new", result);
            }
            InputAction::Commit { .. } => {
                let result = self.jj.commit(&input.value);
                self.report("commit", result);
            }
            InputAction::Bookmark { rev } => {
                if input.value.trim().is_empty() {
                    self.status = Status::error("bookmark: empty name");
                    return;
                }
                let result = self.jj.bookmark_set(input.value.trim(), &rev);
                self.report("bookmark set", result);
            }
            InputAction::RebaseOnto { rev, mode } => {
                if input.value.trim().is_empty() {
                    self.status = Status::error("rebase: empty destination");
                    return;
                }
                let result = self.jj.rebase(&rev, input.value.trim(), mode);
                self.report("rebase", result);
            }
            InputAction::Revset => {
                let trimmed = input.value.trim();
                self.revset = (!trimmed.is_empty()).then(|| trimmed.to_string());
                match self.reload() {
                    Ok(()) => {
                        self.status = Status::info(match &self.revset {
                            Some(r) => format!("revset: {r}"),
                            None => "revset: (jj default)".to_string(),
                        })
                    }
                    Err(err) => self.status = Status::error(format!("revset failed: {err}")),
                }
            }
        }
    }

    /// 確認された操作を実行する。
    pub fn submit_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Abandon { rev } => {
                let result = self.jj.abandon(&rev);
                self.report("abandon", result);
            }
            ConfirmAction::Squash { from, into } => {
                let result = self.jj.squash(&from, &into);
                self.report("squash", result);
            }
            ConfirmAction::Rebase { rev, onto, mode } => {
                let result = self.jj.rebase(&rev, &onto, mode);
                self.report("rebase", result);
            }
            ConfirmAction::RestoreFile { rev, paths, count } => {
                let result = self.jj.restore_file(&rev, &paths);
                self.file_selection.clear();
                self.report_batch("restore", count, 0, result);
            }
            ConfirmAction::UntrackFile {
                ignore: false,
                paths,
                count,
                skipped,
            } => {
                self.file_selection.clear();
                // `I`: `.gitignore` には一切触らず、ignore 済みでないパスは
                // jj の拒否をそのまま報告する (全部か無しか)。
                let result = self.jj.file_untrack(&paths);
                let refused = result.is_err();
                self.report_batch("untrack", count, skipped, result);
                if refused {
                    self.status
                        .text
                        .push_str(" (.gitignore unchanged; use i to ignore + untrack)");
                }
            }
            ConfirmAction::UntrackFile {
                ignore: true,
                paths,
                count,
                skipped,
            } => {
                self.file_selection.clear();
                // `i`: jj は ignore 済みでないパスの untrack を拒否するので
                // `.gitignore` が先。書き込みに失敗したら jj は呼ばない。
                if let Err(err) = crate::gitignore::ignore_paths(self.jj.root(), &paths) {
                    self.status = Status::error(format!("untrack failed: .gitignore: {err}"));
                    return;
                }
                let result = self.jj.file_untrack(&paths);
                if result.is_err() {
                    // 追記は巻き戻さない (再実行は dedupe で冪等)。
                    self.report_batch(
                        "untrack (.gitignore updated, untrack failed)",
                        count,
                        skipped,
                        result,
                    );
                } else {
                    self.report_batch("untrack", count, skipped, result);
                }
            }
            ConfirmAction::Undo => {
                let result = self.jj.undo();
                self.report("undo", result);
            }
            ConfirmAction::Redo => {
                let result = self.jj.redo();
                self.report("redo", result);
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Windows の ConPTY は Press と Release の両方を投げてくる。
        // Release を無視しないと 1 打鍵で 2 回動く。
        if key.kind != KeyEventKind::Press {
            return;
        }
        if let Some(job) = &mut self.remote_job {
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                job.quit_requested = true;
                self.status = Status::info("waiting for the remote operation to finish, then quit");
                self.status_fresh = true;
            }
            return;
        }
        let status_before = self.status.clone();
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.handle_normal(key),
            Mode::Help => {
                // 何のキーでも閉じる。閉じた後の Normal は復帰済み。
                if !matches!(key.code, KeyCode::Char('?')) && key.code != KeyCode::Esc {
                    self.handle_normal(key);
                }
            }
            Mode::Input(input) => self.handle_input(key, input),
            Mode::Confirm(confirm) => self.handle_confirm(key, confirm),
        }
        if self.status != status_before {
            self.status_fresh = true;
        }
    }

    fn handle_input(&mut self, key: KeyEvent, mut input: Input) {
        if self.ai_job.is_some() {
            // 生成中。Esc は prompt ごとではなく生成だけを取り消す — 押した
            // 直後の値に戻るだけで、prompt は開いたまま。それ以外のキーは
            // 捨てる (打った文字が届いた結果で丸ごと消えるのを避ける)。
            // 子プロセス自体は止めないが、結果は受信側ごと捨てるので反映
            // されない。
            if key.code == KeyCode::Esc {
                self.ai_job = None;
                self.status = Status::info("ai: cancelled");
            }
            self.mode = Mode::Input(input);
            return;
        }
        match key.code {
            KeyCode::Esc => self.status = Status::info("cancelled"),
            KeyCode::Enter => self.submit_input(input),
            KeyCode::Backspace => {
                input.value.pop();
                self.mode = Mode::Input(input);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.value.clear();
                self.mode = Mode::Input(input);
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.fill_with_ai(input);
            }
            KeyCode::Char(c) => {
                input.value.push(c);
                self.mode = Mode::Input(input);
            }
            _ => self.mode = Mode::Input(input),
        }
    }

    /// 入力中に Ctrl-g を押したときの処理。
    ///
    /// 対象 rev (Describe/NewChild/Commit が持つもの) の diff を `ai_cmd`
    /// に渡し、返ってきたメッセージで入力値を丸ごと置き換える。`jj diff`
    /// と AI コマンドは背景スレッドで走らせ、結果は [`Self::poll_ai`] が
    /// 反映する。diff を持たない入力 (bookmark 名や revset) や `ai_cmd`
    /// 未設定はその場でエラーにする。
    fn fill_with_ai(&mut self, input: Input) {
        let rev = input.action.ai_rev().map(str::to_string);
        self.mode = Mode::Input(input);
        let Some(rev) = rev else {
            self.status = Status::error("ai: not available for this prompt");
            return;
        };
        let Some(cmd) = self.ai_cmd.clone() else {
            self.status = Status::error(format!("ai: {} is not set", crate::ai::AI_CMD_ENV));
            return;
        };
        let jj = self.jj.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = jj
                .diff_plain(&rev)
                .and_then(|diff| crate::ai::generate_message(&cmd, &diff));
            let _ = tx.send(result);
        });
        self.ai_job = Some(AiJob {
            rx,
            started: Instant::now(),
        });
        self.status = Status::info("ai: generating message… (Esc: cancel)");
    }

    fn handle_confirm(&mut self, key: KeyEvent, confirm: Confirm) {
        match key.code {
            // y と Enter だけを肯定にする。`n`/Esc/その他は全部中止。
            // 破壊的操作なので「知らないキーは中止」に倒す。
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.submit_confirm(confirm.action)
            }
            _ => self.status = Status::info("cancelled"),
        }
    }

    fn handle_normal(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if self.file_selection.is_active() => {
                self.file_selection.clear();
                self.status = Status::info("selection cleared");
            }
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char(' ') if self.focus == Focus::Files => self.toggle_file_mark(),
            KeyCode::Char('V' | 'v') if self.focus == Focus::Files => self.toggle_visual(),
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Log => Focus::Files,
                    Focus::Files => Focus::Diff,
                    Focus::Diff => Focus::Log,
                }
            }
            // 端末は Shift-Tab を独立した `BackTab` として送る (`Tab` +
            // SHIFT 修飾子ではない) — crossterm もそう解釈する。
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Log => Focus::Diff,
                    Focus::Files => Focus::Log,
                    Focus::Diff => Focus::Files,
                }
            }
            KeyCode::Char('d') if ctrl => self.step(PAGE as isize),
            KeyCode::Char('u') if ctrl => self.step(-(PAGE as isize)),
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char(']') if self.focus == Focus::Diff => self.move_file_selection(1),
            KeyCode::Char('[') if self.focus == Focus::Diff => self.move_file_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => match self.focus {
                Focus::Log => self.select_first(),
                Focus::Files => self.select_first_file(),
                Focus::Diff => self.diff_scroll = 0,
            },
            KeyCode::Char('G') | KeyCode::End => match self.focus {
                Focus::Log => self.select_last(),
                Focus::Files => self.select_last_file(),
                Focus::Diff => self.scroll_diff(isize::MAX / 2),
            },
            KeyCode::Char('r') if ctrl => match self.reload_and_clear() {
                Ok(()) => self.status = Status::info("reloaded"),
                Err(err) => self.status = Status::error(format!("reload failed: {err}")),
            },
            KeyCode::Char('/') => {
                self.mode = Mode::Input(Input {
                    prompt: "revset".into(),
                    value: self.revset.clone().unwrap_or_default(),
                    action: InputAction::Revset,
                })
            }
            KeyCode::Char('A') => {
                // 全 change を見たい場面は多い割に `/` で `all()` と打つのは
                // 手数が多い。トグルにして戻れるようにする。
                self.revset = match self.revset.as_deref() {
                    Some("all()") => None,
                    _ => Some("all()".to_string()),
                };
                match self.reload() {
                    Ok(()) => {
                        self.status = Status::info(match &self.revset {
                            Some(r) => format!("revset: {r}"),
                            None => "revset: (jj default)".to_string(),
                        })
                    }
                    Err(err) => self.status = Status::error(format!("revset failed: {err}")),
                }
            }
            KeyCode::Char('m') => self.toggle_mark(),
            KeyCode::Char('R') => {
                self.rebase_mode = self.rebase_mode.next();
                self.status = Status::info(format!("rebase mode: {}", self.rebase_mode.label()));
            }
            KeyCode::Char('L') => {
                self.layout = self.layout.next();
                self.status = Status::info(format!("layout: {}", self.layout.label()));
            }
            KeyCode::Enter => self.edit_selected(),
            KeyCode::Char('f') => self.begin_remote(RemoteAction::Fetch),
            KeyCode::Char('P') => self.begin_remote(RemoteAction::Push),
            KeyCode::Char('d') => self.begin_remote(RemoteAction::Done),
            KeyCode::Char('n') => self.prompt_new(),
            KeyCode::Char('e') => self.prompt_describe(),
            KeyCode::Char('c') => self.prompt_commit(),
            KeyCode::Char('s') => self.confirm_squash(),
            KeyCode::Char('b') => self.prompt_bookmark(),
            KeyCode::Char('a') => self.confirm_abandon(),
            KeyCode::Char('x') => self.confirm_rebase(),
            KeyCode::Char('r') if matches!(self.focus, Focus::Files | Focus::Diff) => {
                self.confirm_restore_file()
            }
            KeyCode::Char('p') if matches!(self.focus, Focus::Files | Focus::Diff) => {
                self.absorb_file()
            }
            KeyCode::Char('i') if matches!(self.focus, Focus::Files | Focus::Diff) => {
                self.confirm_untrack_file(true)
            }
            KeyCode::Char('I') if matches!(self.focus, Focus::Files | Focus::Diff) => {
                self.confirm_untrack_file(false)
            }
            KeyCode::Char('o') if matches!(self.focus, Focus::Files | Focus::Diff) => {
                self.open_selected_file_in_editor()
            }
            KeyCode::Char('u') => {
                self.mode = Mode::Confirm(Confirm {
                    prompt: "undo the last jj operation?".into(),
                    action: ConfirmAction::Undo,
                })
            }
            KeyCode::Char('U') => {
                self.mode = Mode::Confirm(Confirm {
                    prompt: "redo the undone jj operation?".into(),
                    action: ConfirmAction::Redo,
                })
            }
            _ => {}
        }
    }

    /// Ctrl-r: 選択と表示キャッシュを捨ててから reload する。
    fn reload_and_clear(&mut self) -> Result<()> {
        self.file_selection.clear();
        self.rendered_cache.clear();
        self.batch_fallback_commits.clear();
        self.batch_disabled_generation = None;
        self.shown_commit = None;
        self.shown_diff = None;
        self.reload()
    }

    /// files pane の操作対象 (選択の和集合、無ければ cursor の 1 件)。
    fn file_targets(&self) -> Vec<usize> {
        self.file_selection
            .targets(self.file_selected, self.files.len())
    }

    /// Files uses the selection; Diff acts only on the displayed file.
    fn action_file_targets(&self) -> Vec<usize> {
        match self.focus {
            Focus::Files => self.file_targets(),
            Focus::Diff if self.selected_file().is_some() => vec![self.file_selected],
            Focus::Diff | Focus::Log => Vec::new(),
        }
    }

    fn clear_files_action_selection(&mut self) {
        if self.focus == Focus::Files {
            self.file_selection.clear();
        }
    }

    /// `Space`: cursor 行の mark を反転して 1 行下へ。
    fn toggle_file_mark(&mut self) {
        if self.files.is_empty() {
            return;
        }
        self.file_selection.toggle(self.file_selected);
        self.move_file_selection(1);
        self.announce_selection();
    }

    /// `V`/`v`: visual 範囲の開始 / 終了。
    fn toggle_visual(&mut self) {
        if self.files.is_empty() {
            return;
        }
        if self.file_selection.is_visual() {
            self.file_selection.stop_visual();
        } else {
            self.file_selection.start_visual(self.file_selected);
        }
        self.announce_selection();
    }

    fn announce_selection(&mut self) {
        let n = self
            .file_selection
            .count(self.file_selected, self.files.len());
        self.status = Status::info(format!("{n} file(s) selected"));
    }

    /// focus に応じて log の選択 / files の選択 / diff の scroll を動かす。
    fn step(&mut self, delta: isize) {
        match self.focus {
            Focus::Log => self.move_selection(delta),
            Focus::Files => self.move_file_selection(delta),
            Focus::Diff => self.scroll_diff(delta),
        }
    }

    fn toggle_mark(&mut self) {
        let Some(change) = self.selected_change().cloned() else {
            return;
        };
        if self.marked.as_ref().is_some_and(|m| m.id == change.id) {
            self.marked = None;
            self.status = Status::info("mark cleared");
        } else {
            self.status = Status::info(format!("marked {} as target", change.short_id));
            self.marked = Some(change);
        }
    }

    fn edit_selected(&mut self) {
        let Some(change) = self.selected_change().cloned() else {
            return;
        };
        let result = self.jj.edit(&change.id);
        self.report("edit", result);
    }

    fn prompt_new(&mut self) {
        let Some(change) = self.selected_change() else {
            return;
        };
        self.mode = Mode::Input(Input {
            prompt: format!(
                "new child of {} (description, may be empty)",
                change.short_id
            ),
            value: String::new(),
            action: InputAction::NewChild {
                rev: change.id.clone(),
            },
        });
    }

    fn prompt_describe(&mut self) {
        let Some(change) = self.selected_change() else {
            return;
        };
        self.mode = Mode::Input(Input {
            prompt: format!("describe {}", change.short_id),
            // 既存 description を初期値に入れる。1 行目しか持っていないので
            // 複数行 description を上書きすると 2 行目以降は消える。それを
            // 承知で編集したい人向けの操作で、本文込みの編集は `jj describe`
            // を直接使う方が早い。
            value: change.description.clone(),
            action: InputAction::Describe {
                rev: change.id.clone(),
            },
        });
    }

    /// `jj commit` — 選択中の working copy (`@`) を describe して新しい
    /// 空 change に進む、`e` (describe) + `n` (new) の 1 操作版。
    /// `@` 以外を対象にすると `jj commit` の意味と食い違うので、その場合は
    /// プロンプトを出さずにエラーを出す (jj 側の `-r` 指定を待たない)。
    fn prompt_commit(&mut self) {
        let Some(change) = self.selected_change() else {
            return;
        };
        if !change.is_working_copy {
            self.status = Status::error("commit: only the working copy (@) can be committed");
            return;
        }
        self.mode = Mode::Input(Input {
            prompt: format!("commit {} (describe + new)", change.short_id),
            value: change.description.clone(),
            action: InputAction::Commit {
                rev: change.id.clone(),
            },
        });
    }

    fn prompt_bookmark(&mut self) {
        let Some(change) = self.selected_change() else {
            return;
        };
        self.mode = Mode::Input(Input {
            prompt: format!("bookmark name for {}", change.short_id),
            value: change.bookmarks.first().cloned().unwrap_or_default(),
            action: InputAction::Bookmark {
                rev: change.id.clone(),
            },
        });
    }

    fn confirm_squash(&mut self) {
        let Some(change) = self.selected_change().cloned() else {
            return;
        };
        let Some(marked) = self.marked.clone() else {
            self.status = Status::error("squash: mark a destination with `m` first");
            return;
        };
        if marked.id == change.id {
            self.status = Status::error("squash: source and destination are the same change");
            return;
        }
        self.mode = Mode::Confirm(Confirm {
            prompt: format!(
                "squash {} into {} ({})?",
                change.short_id,
                marked.short_id,
                marked.summary()
            ),
            action: ConfirmAction::Squash {
                from: change.id,
                into: marked.id,
            },
        });
    }

    fn confirm_rebase(&mut self) {
        let Some(change) = self.selected_change().cloned() else {
            return;
        };
        match self.marked.clone() {
            Some(marked) => {
                self.mode = Mode::Confirm(Confirm {
                    prompt: format!(
                        "rebase {} onto {} [{}]?",
                        change.short_id,
                        marked.short_id,
                        self.rebase_mode.label()
                    ),
                    action: ConfirmAction::Rebase {
                        rev: change.id,
                        onto: marked.id,
                        mode: self.rebase_mode,
                    },
                });
            }
            // mark が無いときは revset 入力に落とす。`main` や `@-` を
            // 直接打てる方が、一度 mark しに行くより速い場面がある。
            None => {
                self.mode = Mode::Input(Input {
                    prompt: format!(
                        "rebase {} onto (revset) [{}]",
                        change.short_id,
                        self.rebase_mode.label()
                    ),
                    value: String::new(),
                    action: InputAction::RebaseOnto {
                        rev: change.id,
                        mode: self.rebase_mode,
                    },
                });
            }
        }
    }

    fn confirm_abandon(&mut self) {
        let Some(change) = self.selected_change().cloned() else {
            return;
        };
        self.mode = Mode::Confirm(Confirm {
            prompt: format!("abandon {} ({})?", change.short_id, change.summary()),
            action: ConfirmAction::Abandon { rev: change.id },
        });
    }

    /// files pane の選択、または diff pane の表示中ファイルについて、変更を
    /// 破棄する確認ダイアログを開く。`jj restore --changes-in` は内容を
    /// 失う操作なので abandon と同様に確認を挟む。
    fn confirm_restore_file(&mut self) {
        let Some(change) = self.selected_change().cloned() else {
            return;
        };
        let targets = self.action_file_targets();
        // 対象パスはここで固定する。承認後に cursor や選択を再評価しない。
        let plan = plan_restore(&self.files, &targets);
        if plan.count == 0 {
            return;
        }
        let prompt = if plan.count == 1 {
            format!(
                "restore {} — discard this file's change in {}?",
                self.files[targets[0]].path, change.short_id
            )
        } else {
            format!(
                "restore {} files — discard their changes in {}?",
                plan.count, change.short_id
            )
        };
        self.mode = Mode::Confirm(Confirm {
            prompt,
            action: ConfirmAction::RestoreFile {
                rev: change.id,
                paths: plan.paths,
                count: plan.count,
            },
        });
    }

    /// files pane の選択、または diff pane の表示中ファイルの追跡を外す
    /// 確認ダイアログを開く。`jj file untrack` は作業コピーにしか効かない。
    fn confirm_untrack_file(&mut self, ignore: bool) {
        let Some(change) = self.selected_change().cloned() else {
            return;
        };
        let targets = self.action_file_targets();
        if targets.is_empty() {
            return;
        }
        if !change.is_working_copy {
            self.status = Status::error("untrack: select the working-copy change (@) first");
            return;
        }
        let plan = plan_untrack(&self.files, &targets);
        if plan.count == 0 {
            self.status = Status::error("untrack: deleted files cannot be untracked");
            return;
        }
        let what = if plan.count == 1 {
            plan.paths[0].clone()
        } else {
            format!("{} files", plan.count)
        };
        self.mode = Mode::Confirm(Confirm {
            prompt: if ignore {
                format!(
                    "untrack {what} — append to the root .gitignore and stop tracking in the working copy?"
                )
            } else {
                format!(
                    "untrack {what} in the working copy?\nkeep files on disk; leave .gitignore unchanged.\njj may refuse paths that would be auto-tracked again."
                )
            },
            action: ConfirmAction::UntrackFile {
                ignore,
                paths: plan.paths,
                count: plan.count,
                skipped: plan.skipped,
            },
        });
    }

    /// files pane の選択、または diff pane の表示中ファイルの変更を
    /// 祖先の mutable な change へ分散させる (`jj absorb`)。内容は失わず
    /// 置き場所が変わるだけなので、他の非破壊操作 (edit/describe 等) と
    /// 同じく確認なしで即実行する。
    ///
    /// リネーム/コピー (`R`/`C`) は対象外: `jj absorb` は新パスの行を
    /// 祖先の同名パスへ blame で辿るため、パスが変わった行は祖先側に
    /// 一致する行が無いと判定され、渡すパスの組み合わせ次第で
    /// 「何も起きない (無言の no-op)」か「祖先からファイルの中身ごと
    /// 消えて現在の change に丸ごと new として残る (実質的な巻き戻し)」
    /// のどちらかになる — どちらも「選択ファイルの変更が祖先へ移る」
    /// という期待から外れるので、実行前に弾いて理由を出す。
    fn absorb_file(&mut self) {
        let Some(change) = self.selected_change().cloned() else {
            return;
        };
        let targets = self.action_file_targets();
        if targets.is_empty() {
            return;
        }
        let plan = plan_absorb(&self.files, &targets);
        if plan.paths.is_empty() {
            self.status = Status::error(match targets.as_slice() {
                [i] => format!(
                    "absorb: renamed/copied file {} is not supported — jj can't attribute changes across a rename",
                    self.files[*i].path
                ),
                _ => format!(
                    "absorb: all {} selected files are renamed/copied — jj can't attribute changes across a rename",
                    plan.skipped
                ),
            });
            self.clear_files_action_selection();
            return;
        }
        let result = self.jj.absorb_file(&change.id, &plan.paths);
        self.file_selection.clear();
        self.report_batch("absorb", plan.count, plan.skipped, result);
    }

    /// files pane の選択、または diff pane の表示中ファイルを
    /// `EDITOR`/`VISUAL` で開くリクエストを
    /// 立てる。実際の子プロセス起動と端末の明け渡しは
    /// `tui::event_loop` が [`Self::take_pending_editor`] で拾って行う —
    /// ここは Terminal を持っていないので、それ自体は起動しない。
    ///
    /// リネーム/コピーは新パス側 (`target_path`) を開く。削除された
    /// ファイル (`status == 'D'`) はディスクに存在しないので、多くの
    /// エディタは新規ファイルとして開く — それ自体は誤りではないので
    /// 特別扱いしない。
    fn open_selected_file_in_editor(&mut self) {
        let targets = self.action_file_targets();
        if targets.is_empty() {
            return;
        }
        let Some(editor) = self.editor_cmd.clone() else {
            self.status = Status::error(format!(
                "{}/{} is not set",
                crate::editor::EDITOR_ENV,
                crate::editor::VISUAL_ENV
            ));
            return;
        };
        let plan = plan_open(&self.files, &targets);
        if plan.paths.is_empty() {
            self.status = Status::error("open: no file on disk among the selection");
            self.clear_files_action_selection();
            return;
        }
        let root = self.jj.root().to_path_buf();
        self.clear_files_action_selection();
        self.pending_editor = Some(EditorRequest {
            editor,
            paths: plan.paths.iter().map(|p| root.join(p)).collect(),
            skipped: plan.skipped,
        });
    }

    /// [`Self::open_selected_file_in_editor`] が立てたリクエストを取り出す。
    /// `tui::event_loop` が `handle_key` の直後に呼ぶ — `Some` が返ったら
    /// 子プロセスに端末を明け渡す。
    pub fn take_pending_editor(&mut self) -> Option<EditorRequest> {
        self.pending_editor.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{jj_available, repo_or_skip, report_skip, run};
    use std::path::Path;
    use std::process::Command;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn descriptions(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                Row::Change { change, .. } => Some(change.summary()),
                Row::Connector(_) => None,
            })
            .collect()
    }

    fn wait_remote(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while app.remote_progress().is_some() {
            assert!(Instant::now() < deadline, "remote job timed out");
            app.poll_remote();
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn bare_remote(dir: &Path) -> tempfile::TempDir {
        let remote = tempfile::tempdir().unwrap();
        let output = Command::new("git")
            .args(["init", "--bare"])
            .arg(remote.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        run(
            dir,
            &[
                "git",
                "remote",
                "add",
                "origin",
                remote.path().to_str().unwrap(),
            ],
        );
        remote
    }

    fn jj_stdout(dir: &Path, args: &[&str]) -> String {
        let out = crate::testutil::jj_cmd(dir).args(args).output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn remote_refs(remote: &Path) -> Vec<u8> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(remote)
            .args(["for-each-ref", "--format=%(refname) %(objectname)"])
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    }

    #[test]
    fn remote_fetch_refreshes_log_from_every_pane() {
        let (tmp, mut app) = repo_or_skip!();
        let (peer, _peer_app) = repo_or_skip!();
        let remote = bare_remote(tmp.path());
        run(
            peer.path(),
            &[
                "git",
                "remote",
                "add",
                "origin",
                remote.path().to_str().unwrap(),
            ],
        );
        for (index, focus) in [Focus::Log, Focus::Files, Focus::Diff]
            .into_iter()
            .enumerate()
        {
            let description = format!("remote change {index}");
            run(peer.path(), &["describe", "-r", "@-", "-m", &description]);
            run(peer.path(), &["bookmark", "set", "shared", "-r", "@-"]);
            run(
                peer.path(),
                &[
                    "--config",
                    "git.push-new-bookmarks=true",
                    "git",
                    "push",
                    "-b",
                    "shared",
                ],
            );
            app.focus = focus;
            app.handle_key(key(KeyCode::Char('f')));
            assert!(app.remote_progress().is_some());
            wait_remote(&mut app);
            assert_eq!(app.status.kind, StatusKind::Info, "{}", app.status.text);
            assert!(descriptions(&app).contains(&description));
            assert!(!app.working_copy.is_empty());
            assert!(app.status_fresh);
        }
    }

    #[test]
    fn remote_push_updates_tracked_bookmark_from_every_pane() {
        let (tmp, mut app) = repo_or_skip!();
        let remote = bare_remote(tmp.path());
        run(tmp.path(), &["bookmark", "set", "shared", "-r", "@-"]);
        run(
            tmp.path(),
            &[
                "--config",
                "git.push-new-bookmarks=true",
                "git",
                "push",
                "-b",
                "shared",
            ],
        );
        for (index, focus) in [Focus::Log, Focus::Files, Focus::Diff]
            .into_iter()
            .enumerate()
        {
            let before = remote_refs(remote.path());
            run(
                tmp.path(),
                &["describe", "-m", &format!("pushed change {index}")],
            );
            run(tmp.path(), &["bookmark", "set", "shared", "-r", "@"]);
            run(tmp.path(), &["new", "-m", "next change"]);
            app.focus = focus;
            app.handle_key(key(KeyCode::Char('P')));
            wait_remote(&mut app);
            assert_eq!(app.status.kind, StatusKind::Info, "{}", app.status.text);
            assert_ne!(before, remote_refs(remote.path()));
            assert!(descriptions(&app).contains(&format!("pushed change {index}")));
            assert!(app.status.text.contains("shared"), "{}", app.status.text);
        }
    }

    #[test]
    fn remote_push_leaves_new_bookmarks_unpublished_and_preserves_jj_result() {
        let (tmp, mut app) = repo_or_skip!();
        let remote = bare_remote(tmp.path());
        run(
            tmp.path(),
            &["config", "set", "--repo", "git.push-new-bookmarks", "false"],
        );
        for new_bookmark in [false, true] {
            if new_bookmark {
                run(tmp.path(), &["bookmark", "set", "untracked", "-r", "@-"]);
            }
            let expected = crate::testutil::jj_cmd(tmp.path())
                .args(["--no-pager", "--color", "never", "git", "push"])
                .output()
                .unwrap();
            let before = remote_refs(remote.path());
            app.handle_key(key(KeyCode::Char('P')));
            wait_remote(&mut app);
            assert_eq!(
                app.status.kind == StatusKind::Info,
                expected.status.success()
            );
            let stderr = status_detail(&String::from_utf8_lossy(&expected.stderr));
            assert!(app.status.text.contains(&stderr), "{}", app.status.text);
            assert_eq!(before, remote_refs(remote.path()));
            assert!(before.is_empty());
        }
    }

    #[test]
    fn remote_failure_refreshes_log_and_discards_startup_snapshot() {
        let (tmp, mut app) = repo_or_skip!();
        run(
            tmp.path(),
            &["git", "remote", "add", "origin", "missing-remote"],
        );
        run(tmp.path(), &["describe", "-m", "changed before failure"]);
        let (tx, rx) = mpsc::channel();
        tx.send(Ok((Vec::new(), "stale status".into()))).unwrap();
        app.startup_rx = Some(rx);
        app.handle_key(key(KeyCode::Char('f')));
        app.poll_startup();
        wait_remote(&mut app);
        assert_eq!(app.status.kind, StatusKind::Error);
        assert!(app.status.text.starts_with("fetch failed:"));
        assert!(descriptions(&app).contains(&"changed before failure".into()));
        assert_ne!(app.working_copy, "stale status");
        assert!(app.status_fresh);
    }

    fn revision_id(dir: &Path, rev: &str) -> Vec<u8> {
        let output = crate::testutil::jj_cmd(dir)
            .args(["log", "--no-graph", "-r", rev, "-T", "commit_id"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        output.stdout
    }

    #[test]
    fn done_moves_trunk_to_wc_parent_from_every_pane_regardless_of_selection() {
        let (tmp, mut app) = repo_or_skip!();
        run(tmp.path(), &["bookmark", "set", "develop", "-r", "@--"]);
        run(
            tmp.path(),
            &[
                "config",
                "set",
                "--repo",
                "revset-aliases.'trunk()'",
                "develop",
            ],
        );
        run(tmp.path(), &["bookmark", "set", "other", "-r", "@---"]);
        let other_before = revision_id(tmp.path(), "other");
        for focus in [Focus::Log, Focus::Files, Focus::Diff] {
            run(tmp.path(), &["new", "-m", "next change"]);
            app.focus = focus;
            app.selected = app.rows.len() - 1;
            app.handle_key(key(KeyCode::Char('d')));
            assert_eq!(app.remote_progress().unwrap().0, "done");
            wait_remote(&mut app);
            assert_eq!(app.status.kind, StatusKind::Info, "{}", app.status.text);
            assert!(app.status.text.contains("develop -> @-"));
            assert_eq!(
                revision_id(tmp.path(), "develop"),
                revision_id(tmp.path(), "@-")
            );
            assert_eq!(revision_id(tmp.path(), "other"), other_before);
            assert!(descriptions(&app).contains(&"next change".into()));
        }
    }

    #[test]
    fn done_keeps_remote_trunk_name_when_local_bookmark_is_ahead() {
        let (tmp, mut app) = repo_or_skip!();
        let _remote = bare_remote(tmp.path());
        run(tmp.path(), &["bookmark", "set", "main", "-r", "@---"]);
        run(tmp.path(), &["bookmark", "set", "master", "-r", "@--"]);
        run(
            tmp.path(),
            &[
                "--config",
                "git.push-new-bookmarks=true",
                "git",
                "push",
                "-b",
                "master",
            ],
        );
        let remote_before = revision_id(tmp.path(), "master@origin");
        let main_before = revision_id(tmp.path(), "main");
        assert_eq!(revision_id(tmp.path(), "trunk()"), remote_before);
        for _ in 0..2 {
            assert_eq!(app.jj.trunk_bookmark(), "master");
            app.handle_key(key(KeyCode::Char('d')));
            wait_remote(&mut app);
            assert_eq!(app.status.kind, StatusKind::Info, "{}", app.status.text);
            assert!(app.status.text.contains("master -> @-"));
            assert_eq!(
                revision_id(tmp.path(), "master"),
                revision_id(tmp.path(), "@-")
            );
            assert_eq!(revision_id(tmp.path(), "master@origin"), remote_before);
            assert_eq!(revision_id(tmp.path(), "main"), main_before);
            run(tmp.path(), &["new", "-m", "next change before push"]);
        }
    }

    #[test]
    fn done_falls_back_to_main_for_missing_ambiguous_or_failed_trunk_lookup() {
        for trunk in ["root()", "@--", "missing_trunk"] {
            let (tmp, mut app) = repo_or_skip!();
            run(tmp.path(), &["bookmark", "set", "main", "-r", "@--"]);
            run(tmp.path(), &["bookmark", "set", "other", "-r", "@--"]);
            run(
                tmp.path(),
                &["config", "set", "--repo", "revset-aliases.'trunk()'", trunk],
            );
            app.handle_key(key(KeyCode::Char('d')));
            wait_remote(&mut app);
            assert_eq!(app.status.kind, StatusKind::Info, "{}", app.status.text);
            assert!(app.status.text.contains("main -> @-"));
            assert_eq!(
                revision_id(tmp.path(), "main"),
                revision_id(tmp.path(), "@-")
            );
            assert_eq!(
                revision_id(tmp.path(), "other"),
                revision_id(tmp.path(), "@--")
            );
        }
    }

    #[test]
    fn done_noop_or_failure_refreshes_log_without_creating_or_forcing_a_bookmark() {
        for existing in [false, true] {
            let (tmp, mut app) = repo_or_skip!();
            if existing {
                run(tmp.path(), &["bookmark", "set", "main", "-r", "@"]);
            }
            run(
                tmp.path(),
                &[
                    "config",
                    "set",
                    "--repo",
                    "revset-aliases.'trunk()'",
                    "root()",
                ],
            );
            run(
                tmp.path(),
                &["describe", "-m", "changed before done failure"],
            );
            let before = app.jj.bookmarks().unwrap();
            let (tx, rx) = mpsc::channel();
            tx.send(Ok((Vec::new(), "stale status".into()))).unwrap();
            app.startup_rx = Some(rx);
            app.handle_key(key(KeyCode::Char('d')));
            app.poll_startup();
            wait_remote(&mut app);
            if existing {
                assert_eq!(app.status.kind, StatusKind::Error, "{}", app.status.text);
                assert!(app.status.text.starts_with("done failed:"));
                assert!(app.status.text.contains("bookmark move main --to @-"));
            } else {
                assert_eq!(app.status.kind, StatusKind::Info, "{}", app.status.text);
                assert!(app.status.text.starts_with("done: main -> @-:"));
                assert!(app.status.text.contains("No matching bookmarks"));
                assert!(app.status.text.contains("No bookmarks to update"));
            }
            assert!(descriptions(&app).contains(&"changed before done failure".into()));
            assert_ne!(app.working_copy, "stale status");
            assert_eq!(app.jj.bookmarks().unwrap(), before);
            if existing {
                assert_eq!(
                    revision_id(tmp.path(), "main"),
                    revision_id(tmp.path(), "@")
                );
            }
            assert!(app.status_fresh);
        }
    }

    #[test]
    fn done_key_preserves_ctrl_d_half_page_navigation() {
        let (_tmp, mut app) = repo_or_skip!();
        app.handle_key(ctrl('d'));
        assert!(app.remote_progress().is_none());
        assert_eq!(app.selected, PAGE.min(app.rows.len() - 1));
    }

    #[test]
    fn done_moves_main_to_parent_from_any_selection_and_reloads() {
        let (tmp, mut app) = repo_or_skip!();
        run(tmp.path(), &["bookmark", "create", "main", "-r", "@---"]);
        app.reload().unwrap();
        app.focus = Focus::Diff;
        app.handle_key(key(KeyCode::Char('G')));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.remote_progress().is_some());
        wait_remote(&mut app);
        assert_eq!(app.status.kind, StatusKind::Info, "{}", app.status.text);
        assert!(
            app.status.text.starts_with("done: main"),
            "{}",
            app.status.text
        );
        let at = jj_stdout(
            tmp.path(),
            &["log", "-r", "main", "--no-graph", "-T", "description"],
        );
        assert_eq!(at.trim(), "second change");
        assert!(app.status_fresh);
    }

    #[test]
    fn done_refuses_backwards_move_and_still_refreshes() {
        let (tmp, mut app) = repo_or_skip!();
        run(tmp.path(), &["bookmark", "create", "main", "-r", "@"]);
        run(tmp.path(), &["describe", "-m", "changed before done"]);
        app.handle_key(key(KeyCode::Char('d')));
        wait_remote(&mut app);
        assert_eq!(app.status.kind, StatusKind::Error);
        assert!(
            app.status.text.starts_with("done failed:"),
            "{}",
            app.status.text
        );
        assert!(descriptions(&app).contains(&"changed before done".into()));
    }

    #[test]
    fn trunk_bookmark_follows_alias_and_falls_back_to_main() {
        let (tmp, app) = repo_or_skip!();
        assert_eq!(app.jj.trunk_bookmark(), "main");
        run(tmp.path(), &["bookmark", "create", "develop", "-r", "@-"]);
        run(
            tmp.path(),
            &[
                "config",
                "set",
                "--repo",
                "revset-aliases.'trunk()'",
                "develop",
            ],
        );
        assert_eq!(app.jj.trunk_bookmark(), "develop");
        run(
            tmp.path(),
            &["bookmark", "create", "other", "-r", "develop"],
        );
        assert_eq!(app.jj.trunk_bookmark(), "main");
    }

    #[test]
    fn remote_job_blocks_actions_defers_quit_and_preserves_errors() {
        let (_tmp, mut app) = repo_or_skip!();
        let (tx, rx) = mpsc::channel();
        app.remote_job = Some(RemoteJob {
            action: RemoteAction::Push,
            started: Instant::now(),
            rx,
            quit_requested: false,
        });
        let backend = ratatui::backend::TestBackend::new(180, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("push: running… (0s)"), "{screen}");
        assert!(screen.chars().any(|c| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c)));
        let selected = app.selected;
        for code in ['j', 'n', 'f', 'P', 'd', 'u', '?', 'q'] {
            app.handle_key(key(KeyCode::Char(code)));
        }
        assert_eq!(app.selected, selected);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!app.should_quit);
        assert!(app.remote_progress().is_some());
        app.poll_remote();
        assert!(app.remote_progress().is_some());
        assert!(app.remote_quit_requested());
        assert!(app.status.text.contains("then quit"));
        terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            screen.contains("push: running… (0s); will quit when finished"),
            "{screen}"
        );
        tx.send((
            Err(anyhow::anyhow!("original jj error\nHint: keep this")),
            Err(anyhow::anyhow!("refresh error")),
        ))
        .unwrap();
        app.poll_remote();
        assert!(app.should_quit);
        assert!(app.remote_progress().is_none());
        assert_eq!(app.status.kind, StatusKind::Error);
        assert!(
            app.status
                .text
                .contains("original jj error | Hint: keep this")
        );
        assert!(app.status.text.contains("reload failed: refresh error"));
        assert!(app.status_fresh);
    }

    #[test]
    fn log_is_loaded_with_working_copy_marked() {
        let (_tmp, app) = repo_or_skip!();
        let descs = descriptions(&app);
        assert!(
            descs.iter().any(|d| d == "third change"),
            "log missing the working copy change: {descs:?}"
        );
        assert!(descs.iter().any(|d| d == "first change"));
        // 選択は先頭の change 行。root commit まで含めても、connector 行を
        // 指していないこと。
        assert!(app.selected_change().is_some());
        assert!(app.rows.iter().any(|row| matches!(
            row,
            Row::Change { change, .. } if change.is_working_copy
        )));
    }

    #[test]
    fn navigation_skips_connector_rows_and_clamps() {
        let (_tmp, mut app) = repo_or_skip!();
        // connector を挟んでも Change 行だけを辿る。
        let mut seen = Vec::new();
        for _ in 0..10 {
            seen.push(app.selected_change().unwrap().id.clone());
            app.handle_key(key(KeyCode::Char('j')));
        }
        // clamp: 末尾で止まる (wrap しない)。
        let last = app.selected_change().unwrap().id.clone();
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_change().unwrap().id, last);

        app.handle_key(key(KeyCode::Char('g')));
        let first = app.selected_change().unwrap().id.clone();
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_change().unwrap().id, first);
        assert!(seen.iter().collect::<std::collections::BTreeSet<_>>().len() > 1);
    }

    #[test]
    fn describe_rewrites_the_selected_change() {
        let (_tmp, mut app) = repo_or_skip!();
        // 2 番目 (second change) に合わせる。
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.handle_key(key(KeyCode::Char('e')));
        let Mode::Input(input) = &app.mode else {
            panic!("expected input mode, got {:?}", app.mode);
        };
        // 既存 description が初期値。
        assert_eq!(input.value, "second change");
        for c in "-renamed".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(
            descriptions(&app).contains(&"second change-renamed".to_string()),
            "{:?}",
            descriptions(&app)
        );
        // 書き換え後もカーソルは同じ change に残る。
        assert_eq!(
            app.selected_change().unwrap().description,
            "second change-renamed"
        );
    }

    #[test]
    fn commit_describes_the_working_copy_and_advances() {
        let (_tmp, mut app) = repo_or_skip!();
        let before = app.rows.len();
        let old_id = app.selected_change().unwrap().id.clone();
        assert!(app.selected_change().unwrap().is_working_copy);

        app.handle_key(key(KeyCode::Char('c')));
        let Mode::Input(input) = &app.mode else {
            panic!("expected input mode, got {:?}", app.mode);
        };
        // 既存 description (working copy) が初期値。
        assert_eq!(input.value, "third change");
        for c in "-final".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(app.rows.len() > before);
        assert!(
            descriptions(&app).contains(&"third change-final".to_string()),
            "{:?}",
            descriptions(&app)
        );
        // 元の change は describe され、working copy ではなくなっている。
        let old = app
            .rows
            .iter()
            .find_map(|row| match row {
                Row::Change { change, .. } if change.id == old_id => Some(change),
                _ => None,
            })
            .unwrap();
        assert_eq!(old.description, "third change-final");
        assert!(!old.is_working_copy);
        // 新しい working copy が空 change として立っている。
        let wc = app
            .rows
            .iter()
            .find_map(|row| match row {
                Row::Change { change, .. } if change.is_working_copy => Some(change),
                _ => None,
            })
            .unwrap();
        assert!(wc.is_empty);
        assert_eq!(wc.description, "");
        assert_ne!(wc.id, old_id);
    }

    #[test]
    fn commit_with_empty_message_is_allowed() {
        let (_tmp, mut app) = repo_or_skip!();
        app.handle_key(key(KeyCode::Char('c')));
        // working copy の既存 description を消して空メッセージにする。
        app.handle_key(ctrl('u'));
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(descriptions(&app).contains(&"(no description set)".to_string()));
        let wc = app
            .rows
            .iter()
            .find_map(|row| match row {
                Row::Change { change, .. } if change.is_working_copy => Some(change),
                _ => None,
            })
            .unwrap();
        assert!(wc.is_empty);
        assert_eq!(wc.description, "");
    }

    #[test]
    fn commit_refuses_a_change_that_is_not_the_working_copy() {
        let (_tmp, mut app) = repo_or_skip!();
        // 2 番目 (second change, working copy ではない) に合わせる。
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        assert!(!app.selected_change().unwrap().is_working_copy);

        app.handle_key(key(KeyCode::Char('c')));

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.status.kind, StatusKind::Error, "{:?}", app.status);
    }

    #[test]
    fn escape_cancels_input_without_touching_the_repo() {
        let (_tmp, mut app) = repo_or_skip!();
        let before = descriptions(&app);
        app.handle_key(key(KeyCode::Char('e')));
        for c in "junk".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        app.reload().unwrap();
        assert_eq!(descriptions(&app), before);
    }

    #[test]
    fn squash_requires_a_mark_and_then_merges_into_it() {
        let (_tmp, mut app) = repo_or_skip!();
        // mark 無しで s を押しても repo は変わらず、理由が status に出る。
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.status.kind, StatusKind::Error);
        assert!(app.status.text.contains("mark a destination"));
        assert_eq!(app.mode, Mode::Normal);

        // `second change` を destination に mark → `third change` を squash。
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.handle_key(key(KeyCode::Char('m')));
        while app.selected_change().unwrap().description != "third change" {
            app.handle_key(key(KeyCode::Char('k')));
        }
        app.handle_key(key(KeyCode::Char('s')));
        let Mode::Confirm(confirm) = &app.mode else {
            panic!("expected confirm mode, got {:?}", app.mode);
        };
        assert!(confirm.prompt.starts_with("squash "), "{}", confirm.prompt);
        app.handle_key(key(KeyCode::Char('y')));

        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        let descs = descriptions(&app);
        // destination の description が残り (--use-destination-message)、
        // source は空になって abandon される。
        assert!(descs.contains(&"second change".to_string()), "{descs:?}");
        assert!(!descs.contains(&"third change".to_string()), "{descs:?}");
    }

    #[test]
    fn confirm_dialog_rejects_anything_but_yes() {
        let (_tmp, mut app) = repo_or_skip!();
        let before = descriptions(&app);
        app.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(app.mode, Mode::Confirm(_)));
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.mode, Mode::Normal);
        app.reload().unwrap();
        assert_eq!(descriptions(&app), before);
    }

    #[test]
    fn abandon_then_undo_restores_the_change() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(!descriptions(&app).contains(&"second change".to_string()));

        app.handle_key(key(KeyCode::Char('u')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(
            descriptions(&app).contains(&"second change".to_string()),
            "undo did not restore the change: {:?}",
            descriptions(&app)
        );
    }

    #[test]
    fn restore_file_discards_the_files_change() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.sync_diff_blocking(80);
        assert_eq!(app.files.len(), 1, "{:?}", app.files);
        assert_eq!(app.files[0].path, "b.txt");

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('r')));
        let Mode::Confirm(confirm) = &app.mode else {
            panic!("expected confirm mode, got {:?}", app.mode);
        };
        assert!(
            confirm.prompt.starts_with("restore b.txt"),
            "{}",
            confirm.prompt
        );
        app.handle_key(key(KeyCode::Char('y')));

        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        // change_id は書き換えでも変わらないので、選択はそのまま
        // "second change" を指す。description も残る (empty にはなるが
        // abandon はされない)。
        assert_eq!(app.selected_change().unwrap().description, "second change");
        app.sync_diff_blocking(80);
        assert!(app.files.is_empty(), "{:?}", app.files);
    }

    /// working copy に 3 ファイル (x/y/z) を作って files pane にフォーカスする。
    fn app_with_wc_files() -> Option<(tempfile::TempDir, App)> {
        let (tmp, mut app) = crate::testutil::try_repo()?;
        while !app.selected_change().unwrap().is_working_copy {
            app.handle_key(key(KeyCode::Char('k')));
        }
        for name in ["x.txt", "y.txt", "z.txt"] {
            std::fs::write(app.jj.root().join(name), format!("{name}\n")).unwrap();
        }
        app.reload().unwrap();
        app.sync_diff_blocking(80);
        assert_eq!(app.files.len(), 3, "{:?}", app.files);
        app.handle_key(key(KeyCode::Tab));
        Some((tmp, app))
    }

    #[test]
    fn diff_file_navigation_updates_diff_and_preserves_selection() {
        let Some((_tmp, mut app)) = app_with_wc_files() else {
            report_skip();
            return;
        };
        app.file_selection.toggle(2);
        app.file_selection.start_visual(0);
        let selection = app.file_selection.clone();
        app.handle_key(key(KeyCode::Tab));
        for (c, index, path) in [
            (']', 1, "y.txt"),
            (']', 2, "z.txt"),
            ('[', 1, "y.txt"),
            ('[', 0, "x.txt"),
        ] {
            app.diff_scroll = 7;
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT));
            assert_eq!(app.file_selected, index);
            assert_eq!(app.focus, Focus::Diff);
            app.sync_diff_blocking(80);
            assert_eq!(app.diff_scroll, 0);
            assert_eq!(
                app.shown_diff
                    .as_ref()
                    .unwrap()
                    .file_key
                    .raw_cache_path()
                    .as_deref(),
                Some(path)
            );
            assert!(app.diff_plain_text().contains(path));
            assert_eq!(app.file_selection, selection);
            assert_eq!(
                app.file_targets(),
                (0..3).filter(|&i| i <= index || i == 2).collect::<Vec<_>>()
            );
            assert_eq!(app.action_file_targets(), vec![index]);
        }
        // A blocked move must not reset the reading position on the next draw.
        for (index, c) in [(0, '['), (2, ']')] {
            app.file_selected = index;
            app.sync_diff_blocking(80);
            app.diff_scroll = 7;
            app.handle_key(key(KeyCode::Char(c)));
            app.sync_diff_blocking(80);
            assert_eq!(app.file_selected, index);
            assert_eq!(app.diff_scroll, 7);
            assert_eq!(app.focus, Focus::Diff);
        }
        app.files.clear();
        app.file_selected = 0;
        for c in ['[', ']'] {
            app.handle_key(key(KeyCode::Char(c)));
            assert_eq!(app.file_selected, 0);
            assert_eq!(app.focus, Focus::Diff);
        }
    }

    #[test]
    fn diff_file_navigation_is_inactive_in_other_panes_and_prompts() {
        let Some((_tmp, mut app)) = app_with_wc_files() else {
            report_skip();
            return;
        };
        app.file_selected = 1;
        let selected = app.selected;
        for focus in [Focus::Files, Focus::Log] {
            app.focus = focus;
            for c in ['[', ']'] {
                app.handle_key(key(KeyCode::Char(c)));
                assert_eq!(app.file_selected, 1);
                assert_eq!(app.selected, selected);
                assert_eq!(app.focus, focus);
            }
        }
        app.focus = Focus::Diff;
        app.handle_key(key(KeyCode::Char('/')));
        for c in ['[', ']'] {
            app.handle_key(key(KeyCode::Char(c)));
            assert_eq!(app.file_selected, 1);
        }
        let Mode::Input(input) = &app.mode else {
            panic!("expected input mode");
        };
        assert!(input.value.ends_with("[]"));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(app.mode, Mode::Confirm(_)));
        app.handle_key(key(KeyCode::Char(']')));
        assert_eq!(app.file_selected, 1);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.status.text, "cancelled");
    }

    #[test]
    fn space_marks_and_moves_down_and_v_extends_a_range() {
        let Some((_tmp, mut app)) = app_with_wc_files() else {
            report_skip();
            return;
        };
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.file_selected, 1);
        assert!(app.file_selection.is_marked(0));
        app.handle_key(key(KeyCode::Char('V')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.file_selection.selected(app.file_selected, 3),
            vec![0, 1, 2]
        );
        app.handle_key(key(KeyCode::Char('V')));
        assert!(!app.file_selection.is_visual());
        // Esc は選択だけを消し、quit しない。
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.file_selection.is_active());
        assert!(!app.should_quit);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn restore_on_a_selection_confirms_once_and_clears_it() {
        let Some((_tmp, mut app)) = app_with_wc_files() else {
            report_skip();
            return;
        };
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('r')));
        let Mode::Confirm(confirm) = &app.mode else {
            panic!("expected confirm mode, got {:?}", app.mode);
        };
        assert!(confirm.prompt.contains("2 files"), "{}", confirm.prompt);
        // 取消では選択が残る。
        app.handle_key(key(KeyCode::Char('n')));
        assert!(app.file_selection.is_active());
        app.handle_key(key(KeyCode::Char('r')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(!app.file_selection.is_active());
        app.sync_diff_blocking(80);
        assert_eq!(
            app.files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["y.txt"]
        );
    }

    #[test]
    fn open_on_a_selection_requests_every_file_once() {
        let Some((_tmp, mut app)) = app_with_wc_files() else {
            report_skip();
            return;
        };
        app.editor_cmd = Some("true".into());
        app.handle_key(key(KeyCode::Char('V')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('o')));
        let req = app.take_pending_editor().unwrap();
        assert_eq!(req.paths.len(), 2);
        assert!(!app.file_selection.is_active());
    }

    #[test]
    fn diff_actions_ignore_marks_and_visual_ranges() {
        for visual in [false, true] {
            for action in ['o', 'r', 'i', 'I'] {
                let Some((tmp, mut app)) = app_with_wc_files() else {
                    report_skip();
                    return;
                };
                if action == 'I' {
                    std::fs::write(tmp.path().join(".gitignore"), "/y.txt\n").unwrap();
                }
                app.editor_cmd = Some("true".into());
                app.file_selection.toggle(0);
                if visual {
                    app.file_selection.start_visual(0);
                } else {
                    app.file_selection.toggle(2);
                }
                app.handle_key(key(KeyCode::Tab));
                app.handle_key(key(KeyCode::Char(']')));
                app.sync_diff_blocking(80);
                assert_eq!(app.focus, Focus::Diff);
                let selection = app.file_selection.clone();
                assert_eq!(app.action_file_targets(), vec![1]);
                app.handle_key(key(KeyCode::Char(action)));
                if action == 'o' {
                    assert_eq!(
                        app.take_pending_editor().unwrap().paths,
                        vec![app.jj.root().join("y.txt")]
                    );
                    app.reload_after_editor().unwrap();
                    app.sync_diff_blocking(80);
                    assert_eq!(app.file_selection, selection);
                    continue;
                }
                let Mode::Confirm(confirm) = &app.mode else {
                    panic!("expected confirmation: {:?}", app.mode);
                };
                assert!(confirm.prompt.contains("y.txt"), "{}", confirm.prompt);
                match &confirm.action {
                    ConfirmAction::RestoreFile { paths, count, .. }
                    | ConfirmAction::UntrackFile { paths, count, .. } => {
                        assert_eq!(paths, &["y.txt"]);
                        assert_eq!(*count, 1);
                    }
                    other => panic!("unexpected action: {other:?}"),
                }
                app.handle_key(key(KeyCode::Char('n')));
                assert_eq!(app.file_selection, selection);
                assert!(tmp.path().join("y.txt").exists());
                app.handle_key(key(KeyCode::Char(action)));
                // Confirmation keeps its original target even if the cursor changes.
                app.file_selected = 2;
                app.handle_key(key(KeyCode::Char('y')));
                assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
                assert!(!app.file_selection.is_active());
                if matches!(action, 'i' | 'I') {
                    assert_eq!(
                        std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
                        "/y.txt\n"
                    );
                    assert!(tmp.path().join("y.txt").exists());
                } else {
                    assert!(!tmp.path().join("y.txt").exists());
                }
                app.sync_diff_blocking(80);
                assert!(app.files.iter().any(|f| f.path == "x.txt"));
                assert!(app.files.iter().any(|f| f.path == "z.txt"));
                assert!(!app.files.iter().any(|f| f.path == "y.txt"));
            }
        }
    }

    #[test]
    fn diff_actions_with_no_displayed_file_preserve_selection() {
        let (_tmp, mut app) = repo_or_skip!();
        app.focus = Focus::Diff;
        app.files.clear();
        app.file_selection.toggle(0);
        let selection = app.file_selection.clone();
        let status = app.status.clone();
        for action in ['o', 'i', 'I', 'r', 'p'] {
            app.handle_key(key(KeyCode::Char(action)));
            assert!(matches!(app.mode, Mode::Normal));
            assert!(app.take_pending_editor().is_none());
            assert_eq!(app.file_selection, selection);
            assert_eq!(app.status, status);
        }
    }

    #[test]
    fn diff_ineligible_actions_preserve_selection() {
        let Some((_tmp, mut app)) = app_with_wc_files() else {
            report_skip();
            return;
        };
        app.file_selection.toggle(0);
        app.file_selected = 1;
        app.focus = Focus::Diff;
        app.files[1].status = 'R';
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.status.kind, StatusKind::Error);
        assert!(app.status.text.contains("y.txt"));
        assert!(app.file_selection.is_active());
        app.files[1].status = 'D';
        for action in ['i', 'I'] {
            app.handle_key(key(KeyCode::Char(action)));
            assert_eq!(app.status.kind, StatusKind::Error);
            assert!(matches!(app.mode, Mode::Normal));
            assert!(app.file_selection.is_active());
        }
        let Row::Change { change, .. } = &mut app.rows[app.selected] else {
            panic!("expected change");
        };
        change.is_working_copy = false;
        app.files[1].status = 'M';
        for action in ['i', 'I'] {
            app.handle_key(key(KeyCode::Char(action)));
            assert!(app.status.text.contains("working-copy"));
            assert!(matches!(app.mode, Mode::Normal));
            assert!(app.file_selection.is_active());
        }
    }

    #[test]
    fn file_action_keys_are_ignored_in_log_focus() {
        let Some((_tmp, mut app)) = app_with_wc_files() else {
            report_skip();
            return;
        };
        app.file_selection.toggle(0);
        app.file_selection.start_visual(1);
        app.file_selected = 2;
        app.focus = Focus::Log;
        let selection = app.file_selection.clone();
        assert!(app.action_file_targets().is_empty());
        let status_before = app.status.clone();
        for action in ['i', 'I'] {
            app.handle_key(key(KeyCode::Char(action)));
            assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
        }
        app.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
        app.handle_key(key(KeyCode::Char('p')));
        assert!(matches!(app.mode, Mode::Normal), "{:?}", app.mode);
        app.editor_cmd = Some("does-not-matter".to_string());
        app.handle_key(key(KeyCode::Char('o')));
        assert!(app.take_pending_editor().is_none());
        assert_eq!(app.status, status_before);
        assert_eq!(app.file_selection, selection);
    }

    #[test]
    fn untrack_only_refusal_preserves_gitignore_and_the_entire_batch() {
        // No ignore file, unrelated rules, and a mixed ignored/unignored batch.
        for ignore_contents in [None, Some("# keep\r\n/unrelated\r\n"), Some("/x.txt\n")] {
            let Some((tmp, mut app)) = app_with_wc_files() else {
                report_skip();
                return;
            };
            let dir = tmp.path();
            run(
                dir,
                &["config", "set", "--repo", "snapshot.auto-track", "all()"],
            );
            if let Some(contents) = ignore_contents {
                std::fs::write(dir.join(".gitignore"), contents).unwrap();
            }
            if ignore_contents.is_some() {
                app.file_selection.toggle(0);
                app.file_selection.toggle(1);
            }
            app.handle_key(key(KeyCode::Char('I')));
            let Mode::Confirm(confirm) = &app.mode else {
                panic!("expected confirmation: {:?}", app.mode);
            };
            let what = if ignore_contents.is_some() {
                "2 files"
            } else {
                "x.txt"
            };
            assert!(confirm.prompt.contains(what));
            assert!(confirm.prompt.contains(".gitignore unchanged"));
            assert!(confirm.prompt.contains("keep files on disk"));
            assert!(confirm.prompt.contains("jj may refuse"));
            app.handle_key(key(KeyCode::Char('y')));
            assert_eq!(app.status.kind, StatusKind::Error, "{:?}", app.status);
            assert!(app.status.text.contains(".gitignore unchanged"));
            assert!(app.status.text.contains("use i"));
            assert!(!app.file_selection.is_active());
            assert_eq!(
                std::fs::read(dir.join(".gitignore")).ok(),
                ignore_contents.map(|contents| contents.as_bytes().to_vec())
            );
            let listed = jj_stdout(dir, &["file", "list"]);
            for path in ["x.txt", "y.txt", "z.txt"] {
                assert!(listed.lines().any(|line| line == path), "{listed}");
                assert!(dir.join(path).exists());
            }
        }
    }

    #[test]
    fn untrack_only_keeps_existing_ignore_rules_and_untracks_selected_files() {
        for visual in [false, true] {
            let Some((tmp, mut app)) = app_with_wc_files() else {
                report_skip();
                return;
            };
            let dir = tmp.path();
            let contents = b"# handwritten rules\r\n/x.txt\r\n/y.txt\r\n";
            // These files were tracked before the rules were written.
            std::fs::write(dir.join(".gitignore"), contents).unwrap();
            if visual {
                app.file_selection.start_visual(0);
                app.file_selected = 1;
            } else {
                app.file_selection.toggle(0);
                app.file_selection.toggle(1);
            }
            let selection = app.file_selection.clone();
            app.handle_key(key(KeyCode::Char('I')));
            app.handle_key(key(KeyCode::Char('n')));
            assert_eq!(app.file_selection, selection);
            assert!(
                jj_stdout(dir, &["file", "list"])
                    .lines()
                    .any(|line| line == "x.txt")
            );
            assert_eq!(std::fs::read(dir.join(".gitignore")).unwrap(), contents);
            app.handle_key(key(KeyCode::Char('I')));
            app.file_selected = 2; // Confirmation freezes the original target set.
            app.handle_key(key(KeyCode::Char('y')));
            assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
            assert!(!app.file_selection.is_active());
            assert_eq!(std::fs::read(dir.join(".gitignore")).unwrap(), contents);
            let listed = jj_stdout(dir, &["file", "list"]);
            for path in ["x.txt", "y.txt"] {
                assert!(!listed.lines().any(|line| line == path), "{listed}");
                assert!(dir.join(path).exists());
            }
            assert!(listed.lines().any(|line| line == "z.txt"), "{listed}");
        }
    }

    #[test]
    fn untrack_key_ignores_then_untracks_the_working_copy_file() {
        let (tmp, mut app) = repo_or_skip!();
        let dir = tmp.path();
        std::fs::write(dir.join("c.txt"), "c\n").unwrap();
        app.reload().unwrap();
        assert!(app.selected_change().unwrap().is_working_copy);
        app.sync_diff_blocking(80);
        assert_eq!(app.files[0].path, "c.txt", "{:?}", app.files);

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('i')));
        assert!(matches!(app.mode, Mode::Confirm(_)), "{:?}", app.mode);
        app.handle_key(key(KeyCode::Char('y')));

        assert_ne!(app.status.kind, StatusKind::Error, "{:?}", app.status);
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "/c.txt\n"
        );
        let out = crate::testutil::jj_cmd(dir)
            .args(["file", "list"])
            .output()
            .unwrap();
        let listed = String::from_utf8_lossy(&out.stdout);
        assert!(!listed.contains("c.txt"), "{listed}");
        assert!(dir.join("c.txt").exists());
    }

    #[test]
    fn open_in_editor_sets_a_pending_request_for_the_selected_file() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.sync_diff_blocking(80);
        assert_eq!(app.files[0].path, "b.txt");
        app.editor_cmd = Some("code --wait".to_string());

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('o')));

        let req = app.take_pending_editor().expect("pending editor request");
        assert_eq!(req.editor, "code --wait");
        assert_eq!(req.paths, vec![app.jj.root().join("b.txt")]);
        // 消費したら空になる: 次フレームで二重に子プロセスを起動しない。
        assert!(app.take_pending_editor().is_none());
    }

    #[test]
    fn open_in_editor_errors_when_editor_and_visual_are_unset() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.sync_diff_blocking(80);
        app.editor_cmd = None;

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('o')));

        assert!(app.take_pending_editor().is_none());
        assert_eq!(app.status.kind, StatusKind::Error, "{:?}", app.status);
    }

    #[test]
    fn pane_layout_toggle_cycles_and_updates_status() {
        let (_tmp, mut app) = repo_or_skip!();
        assert_eq!(app.layout, PaneLayout::Stacked);
        app.handle_key(key(KeyCode::Char('L')));
        assert_eq!(app.layout, PaneLayout::DiffBelow);
        assert!(app.status.text.contains("diff-below"), "{:?}", app.status);
        app.handle_key(key(KeyCode::Char('L')));
        assert_eq!(app.layout, PaneLayout::Vertical);
        assert!(app.status.text.contains("vertical"), "{:?}", app.status);
        app.handle_key(key(KeyCode::Char('L')));
        assert_eq!(app.layout, PaneLayout::Stacked);
        assert!(app.status.text.contains("stacked"), "{:?}", app.status);
    }

    #[test]
    fn restore_file_on_a_rename_restores_the_original_file() {
        // リネームされたファイルは `path` に `"old => new"` が入る。
        // target_path() (new 側) だけを `jj restore` に渡すと new 側を
        // 消すだけで old 側が復元されず、内容ごと失われる。old/new
        // 両方を渡すことでリネームごと元に戻ることを確認する。
        if !jj_available() {
            report_skip();
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("config.toml"),
            "[user]\nname = \"test\"\nemail = \"test@example.com\"\n",
        )
        .unwrap();
        run(dir, &["git", "init"]);
        std::fs::write(dir.join("old.rs"), "hello\n").unwrap();
        run(dir, &["describe", "-m", "add old.rs"]);
        run(dir, &["new", "-m", "rename"]);
        std::fs::rename(dir.join("old.rs"), dir.join("new.rs")).unwrap();
        run(dir, &["config", "set", "--repo", "user.name", "test"]);
        run(
            dir,
            &["config", "set", "--repo", "user.email", "test@example.com"],
        );

        let jj = Jj::discover(dir).unwrap();
        let mut app = App::new(jj, Some("all()".to_string())).unwrap();
        app.wait_for_startup();

        while app.selected_change().unwrap().description != "rename" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.sync_diff_blocking(80);
        assert_eq!(app.files.len(), 1, "{:?}", app.files);
        assert_eq!(app.files[0].path, "{old.rs => new.rs}");

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Diff);
        app.handle_key(key(KeyCode::Char('r')));
        let Mode::Confirm(confirm) = &app.mode else {
            panic!("expected confirm mode, got {:?}", app.mode);
        };
        assert!(
            confirm.prompt.starts_with("restore {old.rs => new.rs}"),
            "{}",
            confirm.prompt
        );
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);

        app.sync_diff_blocking(80);
        assert!(app.files.is_empty(), "{:?}", app.files);
        assert_eq!(
            std::fs::read_to_string(dir.join("old.rs")).unwrap(),
            "hello\n",
            "restore must bring back the original file, not just delete the renamed one"
        );
        assert!(!dir.join("new.rs").exists());
    }

    #[test]
    fn restore_file_handles_fileset_special_characters_in_the_name() {
        // `jj restore <path>` は path を fileset として解釈する。素のまま渡すと
        // `token%3Adefault@x.com` のような名前が構文エラーになる。
        if !jj_available() {
            report_skip();
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("config.toml"),
            "[user]\nname = \"test\"\nemail = \"test@example.com\"\n",
        )
        .unwrap();
        run(dir, &["git", "init"]);
        run(dir, &["config", "set", "--repo", "user.name", "test"]);
        run(
            dir,
            &["config", "set", "--repo", "user.email", "test@example.com"],
        );
        let name = "token%3Adefault%3Ayukimemi@gmail.com";
        run(dir, &["describe", "-m", "base"]);
        run(dir, &["new", "-m", "add token"]);
        std::fs::write(dir.join(name), "secret\n").unwrap();

        let jj = Jj::discover(dir).unwrap();
        let mut app = App::new(jj, Some("all()".to_string())).unwrap();
        app.wait_for_startup();
        while app.selected_change().unwrap().description != "add token" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.sync_diff_blocking(80);
        assert_eq!(app.files.len(), 1, "{:?}", app.files);

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(app.mode, Mode::Confirm(_)), "{:?}", app.mode);
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(!dir.join(name).exists());
    }

    #[test]
    fn absorb_file_moves_the_change_into_its_mutable_ancestor() {
        assert_absorb_targets(Focus::Files);
    }

    #[test]
    fn diff_absorb_only_moves_the_displayed_file_with_multiple_selections() {
        assert_absorb_targets(Focus::Diff);
    }

    fn assert_absorb_targets(focus: Focus) {
        if !jj_available() {
            report_skip();
            return;
        }
        // absorb は既存行を最後に触った祖先を探す操作なので、共有 repo()
        // fixture の「ファイル追加のみ」では宛先が決まらない。行の編集を
        // 含む専用 repo をここで組み立てる。
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("config.toml"),
            "[user]\nname = \"test\"\nemail = \"test@example.com\"\n",
        )
        .unwrap();
        run(dir, &["git", "init"]);
        std::fs::write(dir.join("f.txt"), "line1\nline2\nline3\n").unwrap();
        std::fs::write(dir.join("g.txt"), "other line\n").unwrap();
        run(dir, &["describe", "-m", "base"]);
        run(dir, &["new", "-m", "top"]);
        std::fs::write(dir.join("f.txt"), "line1\nline2-edited\nline3\n").unwrap();
        std::fs::write(dir.join("g.txt"), "other line edited\n").unwrap();
        run(dir, &["config", "set", "--repo", "user.name", "test"]);
        run(
            dir,
            &["config", "set", "--repo", "user.email", "test@example.com"],
        );

        let jj = Jj::discover(dir).unwrap();
        let mut app = App::new(jj, Some("all()".to_string())).unwrap();
        app.wait_for_startup();

        while app.selected_change().unwrap().description != "top" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.sync_diff_blocking(80);
        assert_eq!(app.files.len(), 2, "{:?}", app.files);
        assert_eq!(app.files[0].path, "f.txt");

        app.handle_key(key(KeyCode::Tab));
        app.file_selection.toggle(1);
        app.file_selection.start_visual(1);
        if focus == Focus::Diff {
            app.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(app.focus, focus);
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);

        assert_eq!(app.selected_change().unwrap().description, "top");
        app.sync_diff_blocking(80);
        if focus == Focus::Diff {
            assert_eq!(app.files.len(), 1, "{:?}", app.files);
            assert_eq!(app.files[0].path, "g.txt");
        } else {
            assert!(app.files.is_empty(), "{:?}", app.files);
        }
        assert!(!app.file_selection.is_active());
    }

    #[test]
    fn absorb_refuses_a_renamed_file_instead_of_silently_doing_nothing() {
        // jj absorb は blame でパスを祖先まで辿るので、リネーム後の新
        // パスだけを渡しても祖先には同名の行が無く「何も起きない」。
        // それを黙って "absorb" 成功扱いにはせず、明示的に断る。
        if !jj_available() {
            report_skip();
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("config.toml"),
            "[user]\nname = \"test\"\nemail = \"test@example.com\"\n",
        )
        .unwrap();
        run(dir, &["git", "init"]);
        std::fs::write(dir.join("old.rs"), "line1\n").unwrap();
        run(dir, &["describe", "-m", "base"]);
        run(dir, &["new", "-m", "rename and edit"]);
        std::fs::rename(dir.join("old.rs"), dir.join("new.rs")).unwrap();
        std::fs::write(dir.join("new.rs"), "line1\nline2\n").unwrap();
        run(dir, &["config", "set", "--repo", "user.name", "test"]);
        run(
            dir,
            &["config", "set", "--repo", "user.email", "test@example.com"],
        );

        let jj = Jj::discover(dir).unwrap();
        let mut app = App::new(jj, Some("all()".to_string())).unwrap();
        app.wait_for_startup();

        while app.selected_change().unwrap().description != "rename and edit" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.sync_diff_blocking(80);
        assert_eq!(app.files.len(), 1, "{:?}", app.files);
        assert_eq!(app.files[0].status, 'R');

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.status.kind, StatusKind::Error, "{:?}", app.status);

        // 何もしていないので、選択中 change の diff はまだ元のまま。
        app.sync_diff_blocking(80);
        assert_eq!(app.files.len(), 1, "{:?}", app.files);
        assert_eq!(app.files[0].status, 'R');
    }

    #[test]
    fn immutable_root_commit_failure_is_surfaced_not_panicked() {
        let (_tmp, mut app) = repo_or_skip!();
        // root commit は log の末尾。abandon は jj 側で必ず拒否される。
        app.handle_key(key(KeyCode::Char('G')));
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status.kind, StatusKind::Error, "{:?}", app.status);
        assert!(
            app.status.text.contains("immutable"),
            "jj の拒否理由が status に出ていない: {}",
            app.status.text
        );
        // 失敗しても log は生きている。
        assert!(app.selected_change().is_some());
    }

    #[test]
    fn rebase_without_mark_prompts_for_a_revset_and_rebases() {
        let (_tmp, mut app) = repo_or_skip!();
        let target = {
            // `first change` の change id を控えておく。
            while app.selected_change().unwrap().description != "first change" {
                app.handle_key(key(KeyCode::Char('j')));
            }
            app.selected_change().unwrap().short_id.clone()
        };
        while app.selected_change().unwrap().description != "third change" {
            app.handle_key(key(KeyCode::Char('k')));
        }
        app.handle_key(key(KeyCode::Char('x')));
        let Mode::Input(input) = &app.mode else {
            panic!("expected revset input, got {:?}", app.mode);
        };
        assert!(matches!(input.action, InputAction::RebaseOnto { .. }));
        for c in target.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        // third change の親が first change になったので、log 上で
        // second change と third change は兄弟になる。
        let rows = app.rows.clone();
        let third = rows
            .iter()
            .find_map(|row| match row {
                Row::Change { change, .. } if change.description == "third change" => Some(change),
                _ => None,
            })
            .expect("third change vanished");
        let parents = jj_parents(app.jj.root(), &third.id);
        assert!(
            parents.iter().any(|p| p.starts_with(&target)),
            "rebase did not reparent: parents={parents:?} target={target}"
        );
    }

    fn jj_parents(root: &Path, rev: &str) -> Vec<String> {
        let out = Command::new("jj")
            .current_dir(root)
            .args([
                "--no-pager",
                "log",
                "-r",
                rev,
                "--no-graph",
                "--color",
                "never",
                "--template",
                r#"parents.map(|p| p.change_id().shortest(8)).join(" ")"#,
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn mark_is_dropped_when_the_marked_change_disappears() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "third change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.handle_key(key(KeyCode::Char('m')));
        assert!(app.marked.is_some());
        // mark したまま、その change を abandon する。
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(
            app.marked.is_none(),
            "消えた change が mark に残ると squash/rebase が毎回失敗する"
        );
    }

    #[test]
    fn diff_pane_is_loaded_for_the_selection_and_scrolls_within_bounds() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.sync_diff_blocking(80);
        let text = app.diff_plain_text();
        assert!(
            text.contains("second change"),
            "diff header missing: {text}"
        );
        assert!(text.contains("b.txt"), "diff body missing: {text}");
        assert_eq!(app.files.len(), 1, "{:?}", app.files);
        assert_eq!(app.files[0].path, "b.txt");

        // Tab は log -> files -> diff の順で回る。
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Files);
        // diff pane に focus を移すと j/k は scroll になる。
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Diff);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.diff_scroll, 1);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.diff_scroll, 0);
        // 上端で止まる (負にならない)。
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.diff_scroll, 0);
        // 下端を超えない。
        app.handle_key(key(KeyCode::Char('G')));
        assert!((app.diff_scroll as usize) < app.diff_line_count());
        // focus が diff のあいだ、log の選択は動かない。
        let selected = app.selected_change().unwrap().id.clone();
        app.handle_key(ctrl('d'));
        assert_eq!(app.selected_change().unwrap().id, selected);
    }

    #[test]
    fn shift_tab_cycles_focus_in_reverse() {
        let (_tmp, mut app) = repo_or_skip!();
        assert_eq!(app.focus, Focus::Log);
        // Shift-Tab は Tab の逆順で回る: log -> diff -> files -> log。
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Diff);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Files);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Log);
        // Tab / Shift-Tab は互いの逆であるべき: 往復すると元に戻る。
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Log);
    }

    #[cfg(unix)]
    fn wrap_cmd() -> &'static str {
        "cat; echo FILTERED_MARKER; echo \"WIDTH:$COLUMNS\""
    }
    #[cfg(windows)]
    fn wrap_cmd() -> &'static str {
        "more & echo FILTERED_MARKER & echo WIDTH:%COLUMNS%"
    }

    #[test]
    fn diff_filter_cmd_rewrites_the_diff_and_receives_the_pane_width() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.diff_filter_cmd = Some(wrap_cmd().to_string());
        app.sync_diff_blocking(42);
        let text = app.diff_plain_text();
        assert!(
            text.contains("FILTERED_MARKER"),
            "diff was not routed through the filter: {text}"
        );
        assert!(text.contains("b.txt"), "original diff content lost: {text}");
        assert!(
            text.contains("WIDTH:42"),
            "pane width not passed through COLUMNS: {text}"
        );
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
    }

    #[test]
    fn diff_filter_cmd_reruns_on_resize_without_changing_the_selection() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.diff_filter_cmd = Some(wrap_cmd().to_string());
        app.sync_diff_blocking(42);
        app.sync_diff_blocking(80);
        let text = app.diff_plain_text();
        assert!(
            text.contains("WIDTH:80"),
            "resize with the same selection did not re-run the filter with the new width: {text}"
        );
    }

    #[test]
    fn broken_diff_filter_cmd_falls_back_to_the_raw_diff() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.diff_filter_cmd = Some("exit 1".to_string());
        app.sync_diff_blocking(80);
        let text = app.diff_plain_text();
        assert!(
            text.contains("b.txt"),
            "diff pane went blank instead of falling back to raw jj output: {text}"
        );
        assert_eq!(
            app.status.kind,
            StatusKind::Error,
            "filter failure should be surfaced in the status bar: {:?}",
            app.status
        );
    }

    #[test]
    fn broken_diff_filter_cmd_does_not_clobber_a_fresh_action_status() {
        let (_tmp, mut app) = repo_or_skip!();
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.diff_filter_cmd = Some("exit 1".to_string());
        // describe 前の commit_id 分のキャッシュを埋めておく。
        app.sync_diff_blocking(80);

        app.handle_key(key(KeyCode::Char('e')));
        for c in "-renamed".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        let status_after_describe = app.status.text.clone();

        // describe で commit_id が変わったので、次の sync_diff は新しい
        // diff を取り直し、フィルタが壊れているので失敗する。それでも
        // 直前の describe 成功メッセージを一度も見せずに消してはいけない。
        app.sync_diff_blocking(80);
        assert_eq!(
            app.status.text, status_after_describe,
            "diff-filter failure overwrote the just-reported action status: {:?}",
            app.status
        );
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
    }

    #[cfg(unix)]
    fn fail_on_new_content_cmd() -> &'static str {
        "if grep -q new-content; then exit 1; else cat; fi"
    }
    #[cfg(windows)]
    fn fail_on_new_content_cmd() -> &'static str {
        "findstr new-content >nul && exit /b 1 || more"
    }

    #[test]
    fn diff_filter_failure_suppressed_after_reload_is_still_reported_next_frame() {
        let (_tmp, mut app) = repo_or_skip!();
        app.diff_filter_cmd = Some(fail_on_new_content_cmd().to_string());
        app.sync_diff_blocking(80);
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);

        // 別プロセスによる working copy への変更を模す。フィルタは
        // "new-content" を含む diff だけ失敗する。
        std::fs::write(app.jj.root().join("b.txt"), "new-content\n").unwrap();
        app.handle_key(ctrl('r'));
        assert_eq!(app.status.text, "reloaded");

        // reload 直後の 1 回はまだ "reloaded" を見せる (直前の操作結果を
        // 一度も見せずに消さないための抑制)。
        app.sync_diff_blocking(80);
        assert_eq!(app.status.text, "reloaded", "{:?}", app.status);

        // だが、選択が変わらなければ次のフレームで必ず再試行され、壊れた
        // フィルタは実際に status bar へ出る。
        app.sync_diff_blocking(80);
        assert_eq!(
            app.status.kind,
            StatusKind::Error,
            "filter failure after reload was silently swallowed forever: {:?}",
            app.status
        );
    }

    #[test]
    fn files_pane_lists_every_changed_file_and_diff_follows_the_selection() {
        let (_tmp, mut app) = repo_or_skip!();
        std::fs::write(app.jj.root().join("x.txt"), "x\n").unwrap();
        std::fs::write(app.jj.root().join("y.txt"), "y\n").unwrap();
        app.reload().unwrap();
        app.sync_diff_blocking(80);

        let mut paths: Vec<&str> = app.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["x.txt", "y.txt"], "{:?}", app.files);

        let first_path = app.files[0].path.clone();
        let second_path = app.files[1].path.clone();
        let first_text = app.diff_plain_text();
        assert!(first_text.contains(&first_path));
        assert!(!first_text.contains(&second_path));

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Files);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.file_selected, 1);
        app.sync_diff_blocking(80);
        let second_text = app.diff_plain_text();
        assert!(second_text.contains(&second_path));
        assert!(!second_text.contains(&first_path));
    }

    #[test]
    fn selected_diffs_resolve_renames_and_literal_paths_in_every_cache_route() {
        let (tmp, mut app) = repo_or_skip!();
        let dir = tmp.path();
        run(
            dir,
            &["config", "set", "--repo", "ui.diff-formatter", ":git"],
        );
        std::fs::create_dir_all(dir.join("nested/old")).unwrap();
        std::fs::write(dir.join("nested/old/{brace.json"), "rename payload\n").unwrap();
        std::fs::write(dir.join("nested/moved.txt"), "move payload\n").unwrap();
        std::fs::write(dir.join("config"), "config rename payload\n").unwrap();
        #[cfg(unix)]
        {
            std::fs::create_dir_all(dir.join("literal => dir")).unwrap();
            std::fs::create_dir_all(dir.join("{literal => dir}")).unwrap();
            std::fs::create_dir_all(dir.join("literal")).unwrap();
            std::fs::create_dir_all(dir.join("dir")).unwrap();
            std::fs::write(
                dir.join("{literal => dir}/a.txt"),
                "braced directory rename\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("literal/{a.txt => b.txt}"),
                "braced filename rename\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("literal => dir/a.txt"),
                "arrow directory payload\n",
            )
            .unwrap();
        }
        run(dir, &["new", "-m", "selected diffs"]);
        std::fs::rename(dir.join("config"), dir.join("config.old")).unwrap();
        std::fs::create_dir(dir.join("config")).unwrap();
        std::fs::write(
            dir.join("config/unrelated.txt"),
            "directory reuse unrelated payload\n",
        )
        .unwrap();
        #[cfg(unix)]
        std::fs::rename(
            dir.join("literal => dir/a.txt"),
            dir.join("literal => dir/b.txt"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            std::fs::rename(
                dir.join("{literal => dir}/a.txt"),
                dir.join("{literal => dir}/b.txt"),
            )
            .unwrap();
            std::fs::rename(
                dir.join("literal/{a.txt => b.txt}"),
                dir.join("dir/{a.txt => b.txt}"),
            )
            .unwrap();
        }
        std::fs::rename(dir.join("nested/moved.txt"), dir.join("moved.txt")).unwrap();
        std::fs::rename(
            dir.join("nested/old/{brace.json"),
            dir.join("nested/settings}base.json"),
        )
        .unwrap();
        let literal_paths = [
            "(selected & ~other).txt",
            "literal {braces}.txt",
            "-leading.txt",
            #[cfg(unix)]
            "{literal => name}.txt",
            #[cfg(unix)]
            "glob:*.txt",
            #[cfg(unix)]
            "quote\"and\\slash.txt",
        ];
        for path in &literal_paths {
            std::fs::write(dir.join(path), format!("payload for {path}\n")).unwrap();
        }
        std::fs::write(dir.join("unrelated.txt"), "unrelated payload\n").unwrap();
        app.reload().unwrap();
        app.wait_for_startup();
        app.selected = app
            .rows
            .iter()
            .position(|row| {
                matches!(row,
                    Row::Change { change, .. } if change.description == "selected diffs"
                )
            })
            .unwrap();
        let commit_id = app.selected_change().unwrap().full_commit_id.clone();
        let files = app.jj.diff_summary("@").unwrap();
        assert!(files.iter().any(|file| file.status == 'R'), "{files:?}");
        #[cfg(unix)]
        {
            let ambiguous: Vec<_> = files
                .iter()
                .filter(|file| file.path == "{literal => dir}/{a.txt => b.txt}")
                .collect();
            assert_eq!(ambiguous.len(), 2, "{files:?}");
            assert!(
                ambiguous.iter().any(|file| file.diff_paths()
                    == ["{literal => dir}/a.txt", "{literal => dir}/b.txt"])
            );
            assert!(
                ambiguous.iter().any(|file| file.diff_paths()
                    == ["literal/{a.txt => b.txt}", "dir/{a.txt => b.txt}"])
            );
        }
        let targets: Vec<_> = files
            .iter()
            .filter(|file| {
                file.path != "unrelated.txt" && file.destination_path != "config/unrelated.txt"
            })
            .cloned()
            .collect();

        // No commit cache, an existing metadata-only entry, then warm rendered cache.
        for route in 0..3 {
            if route < 2 {
                app.diff_cache.forget(&commit_id);
                app.rendered_cache.clear();
                app.shown_commit = None;
                app.shown_diff = None;
            }
            if route == 1 {
                app.diff_cache.insert(
                    commit_id.clone(),
                    CommitCache::new(files.clone(), app.jj.show("@").unwrap()),
                );
            }
            app.sync_diff_blocking(80);
            for file in &targets {
                app.file_selected = app.files.iter().position(|f| f == file).unwrap();
                app.sync_diff_blocking(80);
                let text = app.diff_plain_text();
                assert!(
                    text.contains("diff --git"),
                    "configured formatter missing, route {route}: {text}"
                );
                assert!(!text.contains("unrelated payload"), "route {route}: {text}");
                assert!(!text.contains("unrelated.txt"), "route {route}: {text}");
                if file.status == 'R' && file.target_path() == "moved.txt" {
                    assert!(
                        text.contains("rename from nested/moved.txt"),
                        "route {route}: {text}"
                    );
                    assert!(
                        text.contains("rename to moved.txt"),
                        "route {route}: {text}"
                    );
                } else if file.status == 'R' && file.target_path() == "literal => dir/b.txt" {
                    assert!(
                        text.contains("rename from literal => dir/a.txt"),
                        "route {route}: {text}"
                    );
                    assert!(
                        text.contains("rename to literal => dir/b.txt"),
                        "route {route}: {text}"
                    );
                } else if file.status == 'R' && file.target_path() == "{literal => dir}/b.txt" {
                    assert!(
                        text.contains("rename from {literal => dir}/a.txt"),
                        "route {route}: {text}"
                    );
                    assert!(
                        text.contains("rename to {literal => dir}/b.txt"),
                        "route {route}: {text}"
                    );
                    assert!(
                        !text.contains("rename from literal/{a.txt => b.txt}"),
                        "route {route}: {text}"
                    );
                } else if file.status == 'R' && file.target_path() == "dir/{a.txt => b.txt}" {
                    assert!(
                        text.contains("rename from literal/{a.txt => b.txt}"),
                        "route {route}: {text}"
                    );
                    assert!(
                        text.contains("rename to dir/{a.txt => b.txt}"),
                        "route {route}: {text}"
                    );
                    assert!(
                        !text.contains("rename from {literal => dir}/a.txt"),
                        "route {route}: {text}"
                    );
                } else if file.status == 'R' && file.target_path() == "config.old" {
                    assert!(text.contains("rename from config"), "route {route}: {text}");
                    assert!(
                        text.contains("rename to config.old"),
                        "route {route}: {text}"
                    );
                } else if file.status == 'R' {
                    assert!(
                        text.contains("rename from nested/old/{brace.json"),
                        "route {route}: {text}"
                    );
                    assert!(
                        text.contains("rename to nested/settings}base.json"),
                        "route {route}: {text}"
                    );
                } else {
                    assert!(text.contains("payload for"), "route {route}: {text}");
                    assert!(!text.contains("rename from"), "route {route}: {text}");
                }
            }
        }
        let reused_directory_diff = app
            .jj
            .diff("@", &["config/unrelated.txt".to_owned()])
            .unwrap();
        assert!(reused_directory_diff.contains("directory reuse unrelated payload"));
        assert!(!reused_directory_diff.contains("rename from config"));

        // Changing the configured formatter also applies to a fresh selected diff.
        run(
            dir,
            &[
                "config",
                "set",
                "--repo",
                "ui.diff-formatter",
                ":color-words",
            ],
        );
        let raw = app.jj.diff("@", &["unrelated.txt".to_owned()]).unwrap();
        assert!(!raw.contains("diff --git"), "{raw}");
        assert!(raw.contains("unrelated payload"), "{raw}");
        let whole = app.jj.diff("@", &[]).unwrap();
        assert!(whole.contains("unrelated payload"));
        assert!(whole.contains("payload for"));

        // Reusing the old name must not leak its replacement into the new file's diff.
        std::fs::write(dir.join("nested/old/{brace.json"), "replacement payload\n").unwrap();
        let files = app.jj.diff_summary("@").unwrap();
        let selected = files
            .iter()
            .find(|file| dir.join(file.target_path()) == dir.join("nested/settings}base.json"))
            .unwrap();
        let raw = app.jj.diff("@", &selected.diff_paths()).unwrap();
        assert!(!raw.contains("replacement payload"), "{selected:?}: {raw}");
    }

    #[test]
    fn revisiting_a_commit_reuses_the_cached_diff_without_calling_jj_again() {
        let (_tmp, mut app) = repo_or_skip!();

        // 先頭 (working copy = third change) の diff/files を一度取ってキャッシュさせる。
        app.sync_diff_blocking(80);
        let first_id = app.selected_change().unwrap().full_commit_id.clone();
        let first_text = app.diff_plain_text();
        let first_files: Vec<String> = app.files.iter().map(|f| f.path.clone()).collect();

        // 別 commit (second change) へ移動して files/diff を取り直させる。
        app.handle_key(key(KeyCode::Char('j')));
        app.sync_diff_blocking(80);
        assert_ne!(app.selected_change().unwrap().full_commit_id, first_id);

        // ここで repo 自体を消す: これ以降に本当に `jj` を叩けば必ず失敗する。
        std::fs::remove_dir_all(app.jj.root()).unwrap();

        // 元の commit に戻る。commit_id は content-addressed で不変なので、
        // キャッシュヒットするなら jj を一切呼ばずに同じ内容が復元されるはず。
        app.handle_key(key(KeyCode::Char('k')));
        app.sync_diff_blocking(80);
        assert_eq!(app.selected_change().unwrap().full_commit_id, first_id);
        assert_eq!(app.diff_plain_text(), first_text);
        let files_again: Vec<String> = app.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(files_again, first_files);
    }

    #[test]
    fn jj_failures_are_not_cached_and_are_retried_on_the_next_view() {
        let (_tmp, mut app) = repo_or_skip!();
        // 先頭 (working copy) はここでキャッシュさせておき、壊す対象から外す。
        app.sync_diff_blocking(80);
        // 壊す対象 (second change) へ移動するが、まだ sync_diff は呼ばない。
        app.handle_key(key(KeyCode::Char('j')));
        let commit_id = app.selected_change().unwrap().full_commit_id.clone();
        app.diff_cache.forget(&commit_id);

        // repo を一時的に隠して `jj diff --summary`/`jj show` を失敗させる。
        let jj_dir = app.jj.root().join(".jj");
        let hidden = app.jj.root().join(".jj-hidden");
        std::fs::rename(&jj_dir, &hidden).unwrap();
        app.sync_diff_blocking(80);
        std::fs::rename(&hidden, &jj_dir).unwrap();

        assert!(
            !app.diff_cache.contains(&commit_id),
            "jj の失敗が成功データとしてキャッシュに残っている"
        );
        assert!(
            !app.header.lines.is_empty(),
            "失敗理由が header に出ていない (空のまま)"
        );

        // repo を戻して別 commit を経由 → 同じ commit に戻ると、
        // キャッシュされていないので jj を叩き直し、今度は成功する。
        app.handle_key(key(KeyCode::Char('j')));
        app.sync_diff_blocking(80);
        app.handle_key(key(KeyCode::Char('k')));
        app.sync_diff_blocking(80);
        assert!(
            app.diff_cache.contains(&commit_id),
            "リトライ後は成功データがキャッシュされているはず"
        );
    }

    #[test]
    fn reload_discards_a_pending_startup_receiver() {
        let (_tmp, mut app) = repo_or_skip!();
        // 起動時のバックグラウンド `jj log` がまだ終わっていない状態を
        // 再現する (テストの `repo()` は `wait_for_startup` 済みなので、
        // ここで改めて private な `begin_reload` を呼んで re-arm する)。
        app.begin_reload();
        assert!(app.is_loading(), "re-arm できていない");

        // reload (Ctrl-r 相当) を呼んだ時点で、後から届く可能性のある
        // 古い起動結果を待つ理由はなくなる — 捨てられているはず。
        app.reload().unwrap();
        assert!(
            !app.is_loading(),
            "reload が pending の startup receiver を破棄していない: \
             後から届く古い結果で reload の結果が上書きされ得る"
        );
    }

    #[test]
    fn key_release_events_are_ignored() {
        // ConPTY は Press/Release を両方投げる。Release で二重に動くと
        // Windows だけカーソルが 2 行飛ぶ。
        let (_tmp, mut app) = repo_or_skip!();
        let start = app.selected_change().unwrap().id.clone();
        let mut release = key(KeyCode::Char('j'));
        release.kind = KeyEventKind::Release;
        app.handle_key(release);
        assert_eq!(app.selected_change().unwrap().id, start);
    }

    #[test]
    fn revset_toggle_switches_between_all_and_default() {
        let (_tmp, mut app) = repo_or_skip!();
        app.revset = None;
        app.reload().unwrap();
        app.handle_key(key(KeyCode::Char('A')));
        assert_eq!(app.revset.as_deref(), Some("all()"));
        app.handle_key(key(KeyCode::Char('A')));
        assert_eq!(app.revset, None);
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
    }

    #[test]
    fn new_child_with_empty_message_is_allowed() {
        let (_tmp, mut app) = repo_or_skip!();
        let before = app.rows.len();
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(app.rows.len() > before);
        // 新しい working copy が空 change として立っている。
        let wc = app
            .rows
            .iter()
            .find_map(|row| match row {
                Row::Change { change, .. } if change.is_working_copy => Some(change),
                _ => None,
            })
            .unwrap();
        assert!(wc.is_empty);
        assert_eq!(wc.description, "");
    }

    #[cfg(unix)]
    fn echo_message_cmd(msg: &str) -> String {
        format!("echo '{msg}'")
    }
    #[cfg(windows)]
    fn echo_message_cmd(msg: &str) -> String {
        format!("echo {msg}")
    }
    /// 数秒かかってから `late` を返す AI コマンド。生成中の状態を観測する用。
    #[cfg(unix)]
    fn slow_message_cmd() -> String {
        "sleep 3; echo late".into()
    }
    #[cfg(windows)]
    fn slow_message_cmd() -> String {
        "ping -n 4 127.0.0.1 >nul & echo late".into()
    }

    #[test]
    fn ai_ctrl_g_fills_the_describe_prompt_from_the_diff() {
        let (_tmp, mut app) = repo_or_skip!();
        app.ai_cmd = Some(echo_message_cmd("feat: ai generated message"));
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.handle_key(key(KeyCode::Char('e')));
        app.handle_key(ctrl('g'));
        // 生成は背景で走る: 押した直後は生成中として見えている。
        assert!(app.ai_elapsed().is_some());
        app.wait_for_ai();
        assert!(app.ai_elapsed().is_none());
        let Mode::Input(input) = &app.mode else {
            panic!("expected input mode, got {:?}", app.mode);
        };
        assert_eq!(input.value, "feat: ai generated message");
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);

        // Enter で、生成された値がそのまま describe される。
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.status.kind, StatusKind::Info, "{:?}", app.status);
        assert!(
            descriptions(&app).contains(&"feat: ai generated message".to_string()),
            "{:?}",
            descriptions(&app)
        );
    }

    #[test]
    fn ai_generation_ignores_typing_and_esc_cancels_only_the_generation() {
        let (_tmp, mut app) = repo_or_skip!();
        app.ai_cmd = Some(slow_message_cmd());
        app.handle_key(key(KeyCode::Char('e')));
        let Mode::Input(input) = &app.mode else {
            panic!("expected input mode, got {:?}", app.mode);
        };
        let before = input.value.clone();
        app.handle_key(ctrl('g'));
        assert!(app.ai_elapsed().is_some());

        // 生成中の打鍵や Enter は捨てる — 届いた結果で消えるか、生成途中の
        // 値で describe してしまうため。
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Enter));
        let Mode::Input(input) = &app.mode else {
            panic!("expected input mode, got {:?}", app.mode);
        };
        assert_eq!(input.value, before);
        assert!(app.ai_elapsed().is_some());

        // Esc は生成だけを止め、prompt は開いたまま。
        app.handle_key(key(KeyCode::Esc));
        assert!(app.ai_elapsed().is_none());
        let Mode::Input(input) = &app.mode else {
            panic!("expected input mode, got {:?}", app.mode);
        };
        assert_eq!(input.value, before);
        assert!(app.status.text.contains("cancelled"), "{:?}", app.status);
    }

    #[test]
    fn ai_ctrl_g_is_unavailable_for_bookmark_prompt() {
        let (_tmp, mut app) = repo_or_skip!();
        app.ai_cmd = Some(echo_message_cmd("should not be used"));
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(ctrl('g'));
        let Mode::Input(input) = &app.mode else {
            panic!("expected input mode, got {:?}", app.mode);
        };
        // bookmark 入力は diff と無関係なので、値は変わらない。
        assert_eq!(input.value, "");
        assert_eq!(app.status.kind, StatusKind::Error, "{:?}", app.status);
        assert!(
            app.status.text.contains("not available"),
            "{:?}",
            app.status
        );
    }

    #[test]
    fn ai_ctrl_g_without_configured_command_reports_error() {
        let (_tmp, mut app) = repo_or_skip!();
        app.ai_cmd = None;
        app.handle_key(key(KeyCode::Char('e')));
        app.handle_key(ctrl('g'));
        assert_eq!(app.status.kind, StatusKind::Error, "{:?}", app.status);
        assert!(
            app.status.text.contains("SHIKIGAMI_AI_CMD"),
            "{:?}",
            app.status
        );
        // 失敗しても入力は消えず、キャンセルもされない。
        assert!(matches!(app.mode, Mode::Input(_)));
    }

    // ─── Async diff pipeline & delta-v1 batching deterministic tests ───

    struct Gate {
        state: std::sync::Mutex<GateState>,
        cv: std::sync::Condvar,
    }

    struct GateState {
        entered: usize,
        released: usize,
    }

    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: std::sync::Mutex::new(GateState {
                    entered: 0,
                    released: 0,
                }),
                cv: std::sync::Condvar::new(),
            })
        }

        fn arrive_and_wait(&self) {
            let mut guard = self.state.lock().unwrap();
            let ticket = guard.entered + 1;
            guard.entered = ticket;
            self.cv.notify_all();
            while guard.released < ticket {
                guard = self.cv.wait(guard).unwrap();
            }
        }

        fn wait_entered(&self, count: usize) {
            let mut guard = self.state.lock().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while guard.entered < count {
                let rem = deadline.saturating_duration_since(Instant::now());
                assert!(!rem.is_zero(), "timed out waiting for worker to enter gate");
                let (g, _) = self.cv.wait_timeout(guard, rem).unwrap();
                guard = g;
            }
        }

        fn release_next(&self) {
            let mut guard = self.state.lock().unwrap();
            guard.released += 1;
            self.cv.notify_all();
        }

        fn release_all(&self) {
            let mut guard = self.state.lock().unwrap();
            guard.released = usize::MAX;
            self.cv.notify_all();
        }
    }

    struct FakeBackend {
        formatter: std::sync::Mutex<String>,
        files: std::sync::Mutex<std::collections::HashMap<String, Vec<DiffFile>>>,
        headers: std::sync::Mutex<std::collections::HashMap<String, String>>,
        git_patches: std::sync::Mutex<std::collections::HashMap<String, String>>,
        per_file_diffs: std::sync::Mutex<std::collections::HashMap<(String, Vec<String>), String>>,
        corrupt_delta: std::sync::atomic::AtomicBool,
        fail_delta: std::sync::atomic::AtomicBool,
        delta_gate: Option<Arc<Gate>>,
        filter_gate: Option<Arc<Gate>>,
        diff_git_calls: std::sync::atomic::AtomicUsize,
        delta_batch_calls: std::sync::atomic::AtomicUsize,
        diff_file_calls: std::sync::atomic::AtomicUsize,
        filter_calls: std::sync::atomic::AtomicUsize,
        filter_seen_inputs: std::sync::Mutex<Vec<(String, u16)>>,
    }

    impl FakeBackend {
        fn new(formatter: &str) -> Arc<Self> {
            Arc::new(Self {
                formatter: std::sync::Mutex::new(formatter.to_string()),
                files: std::sync::Mutex::new(std::collections::HashMap::new()),
                headers: std::sync::Mutex::new(std::collections::HashMap::new()),
                git_patches: std::sync::Mutex::new(std::collections::HashMap::new()),
                per_file_diffs: std::sync::Mutex::new(std::collections::HashMap::new()),
                corrupt_delta: std::sync::atomic::AtomicBool::new(false),
                fail_delta: std::sync::atomic::AtomicBool::new(false),
                delta_gate: None,
                filter_gate: None,
                diff_git_calls: std::sync::atomic::AtomicUsize::new(0),
                delta_batch_calls: std::sync::atomic::AtomicUsize::new(0),
                diff_file_calls: std::sync::atomic::AtomicUsize::new(0),
                filter_calls: std::sync::atomic::AtomicUsize::new(0),
                filter_seen_inputs: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn add_commit(
            &self,
            commit_id: &str,
            files: Vec<DiffFile>,
            header: &str,
            git_patch: &str,
            per_file: Vec<(Vec<String>, String)>,
        ) {
            self.files
                .lock()
                .unwrap()
                .insert(commit_id.to_string(), files);
            self.headers
                .lock()
                .unwrap()
                .insert(commit_id.to_string(), header.to_string());
            self.git_patches
                .lock()
                .unwrap()
                .insert(commit_id.to_string(), git_patch.to_string());
            let mut map = self.per_file_diffs.lock().unwrap();
            for (paths, diff) in per_file {
                map.insert((commit_id.to_string(), paths), diff);
            }
        }
    }

    impl DiffBackend for FakeBackend {
        fn diff_summary(&self, commit_id: &str) -> Result<Vec<DiffFile>> {
            Ok(self
                .files
                .lock()
                .unwrap()
                .get(commit_id)
                .cloned()
                .unwrap_or_default())
        }

        fn show(&self, commit_id: &str) -> Result<String> {
            Ok(self
                .headers
                .lock()
                .unwrap()
                .get(commit_id)
                .cloned()
                .unwrap_or_default())
        }

        fn diff_formatter(&self) -> Result<String> {
            Ok(self.formatter.lock().unwrap().clone())
        }

        fn diff_git_full(&self, commit_id: &str) -> Result<String> {
            self.diff_git_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.git_patches
                .lock()
                .unwrap()
                .get(commit_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing fake git patch for {commit_id}"))
        }

        fn diff_file(&self, commit_id: &str, paths: &[String]) -> Result<String> {
            self.diff_file_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self
                .per_file_diffs
                .lock()
                .unwrap()
                .get(&(commit_id.to_string(), paths.to_vec()))
                .cloned()
                .unwrap_or_default())
        }

        fn run_delta_batch(
            &self,
            _config: &DeltaBatchConfig,
            framed: &crate::diff_batch::FramedBatch,
            width: u16,
        ) -> Result<String> {
            self.delta_batch_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(gate) = &self.delta_gate {
                gate.arrive_and_wait();
            }
            if self.fail_delta.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("simulated delta failure");
            }
            if self.corrupt_delta.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok("STRAY_OUTSIDE_MARKERS\n".to_string());
            }
            // Echo markers untouched while tagging each section with ANSI + width.
            let mut out = String::new();
            for line in framed.input.lines() {
                if line.starts_with("commit ") {
                    out.push_str(line);
                    out.push('\n');
                } else {
                    out.push_str(&format!("\x1b[32mDELTA[w={width}]: {line}\x1b[0m\n"));
                }
            }
            Ok(out)
        }

        fn run_diff_filter(&self, _cmd: &str, raw: &str, width: u16) -> Result<String> {
            self.filter_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.filter_seen_inputs
                .lock()
                .unwrap()
                .push((raw.to_string(), width));
            if let Some(gate) = &self.filter_gate {
                gate.arrive_and_wait();
            }
            Ok(format!("\x1b[36mFILTER[w={width}]: {raw}\x1b[0m"))
        }
    }

    fn fake_app(backend: Arc<FakeBackend>, commits: &[(&str, &str)]) -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".jj")).unwrap();
        let jj = Jj::discover(tmp.path()).unwrap();
        let mut app = App::with_backend(jj, None, backend, false);
        app.diff_filter_cmd = None;
        app.diff_batch_config = None;
        let rows: Vec<Row> = commits
            .iter()
            .enumerate()
            .map(|(i, (cid, desc))| Row::Change {
                graph: "@  ".to_string(),
                change: Change {
                    id: format!("chg{i}"),
                    short_id: format!("chg{i}"),
                    commit_id: cid.chars().take(8).collect(),
                    full_commit_id: (*cid).to_string(),
                    is_working_copy: i == 0,
                    is_empty: false,
                    has_conflict: false,
                    is_immutable: false,
                    author: "test".to_string(),
                    timestamp: "2026-11-21 00:00".to_string(),
                    bookmarks: Vec::new(),
                    description: (*desc).to_string(),
                },
            })
            .collect();
        app.apply_rows(rows, "chg0".to_string());
        app.status = Status::info("? for help");
        (tmp, app)
    }

    type SampleCommit = (Vec<DiffFile>, String, Vec<(Vec<String>, String)>);

    fn sample_multi_file_commit() -> SampleCommit {
        let files = vec![
            DiffFile {
                status: 'M',
                path: "alpha.rs".into(),
                source_path: "alpha.rs".into(),
                destination_path: "alpha.rs".into(),
            },
            DiffFile {
                status: 'R',
                path: "{old space.txt => new space.txt}".into(),
                source_path: "old space.txt".into(),
                destination_path: "new space.txt".into(),
            },
            DiffFile {
                status: 'A',
                path: "日本語.txt".into(),
                source_path: "日本語.txt".into(),
                destination_path: "日本語.txt".into(),
            },
            DiffFile {
                status: 'M',
                path: "exec.sh".into(),
                source_path: "exec.sh".into(),
                destination_path: "exec.sh".into(),
            },
        ];
        let git_patch = concat!(
            "diff --git a/alpha.rs b/alpha.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/alpha.rs\n",
            "+++ b/alpha.rs\n",
            "@@ -1 +1 @@\n",
            "-old_alpha\n",
            "+new_alpha\n",
            "diff --git a/old space.txt b/new space.txt\n",
            "rename from old space.txt\n",
            "rename to new space.txt\n",
            "diff --git a/日本語.txt b/日本語.txt\n",
            "new file mode 100644\n",
            "index 0000000..3333333\n",
            "--- /dev/null\n",
            "+++ b/日本語.txt\n",
            "@@ -0,0 +1 @@\n",
            "+こんにちは\n",
            "diff --git a/exec.sh b/exec.sh\n",
            "old mode 100644\n",
            "new mode 100755\n",
        )
        .to_string();
        let per_file = vec![
            (vec!["alpha.rs".into()], "RAW_ALPHA\n".into()),
            (
                vec!["old space.txt".into(), "new space.txt".into()],
                "RAW_RENAME\n".into(),
            ),
            (vec!["日本語.txt".into()], "RAW_UTF8\n".into()),
            (vec!["exec.sh".into()], "RAW_MODE\n".into()),
        ];
        (files, git_patch, per_file)
    }

    #[test]
    fn batch_worker_reuses_raw_and_formatted_results_before_ui_receives_them() {
        use std::sync::atomic::Ordering;

        let gate = Gate::new();
        let mut backend = Arc::try_unwrap(FakeBackend::new(":git")).ok().unwrap();
        backend.delta_gate = Some(Arc::clone(&gate));
        let backend = Arc::new(backend);
        let (files, patch, per_file) = sample_multi_file_commit();
        let commit_id = "1111111111111111111111111111111111111111";
        backend.add_commit(commit_id, files.clone(), "", &patch, per_file);
        let workers = DiffWorkers::new(Arc::clone(&backend) as Arc<dyn DiffBackend>);
        let mut request = BodyRequest {
            commit_id: commit_id.into(),
            width: 100,
            generation: 0,
            strategy: BodyStrategy::BatchDelta {
                config: DeltaBatchConfig::delta_v1(),
                selected_file: files.first().cloned(),
                files,
                cached_selected_raw_diff: None,
            },
        };
        workers.submit_body(request.clone());
        gate.wait_entered(1);
        request.width = 140;
        workers.submit_body(request.clone());
        gate.release_all();

        let wait_idle = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while workers.is_busy() {
                assert!(Instant::now() < deadline, "diff worker did not settle");
                thread::sleep(Duration::from_millis(1));
            }
        };
        wait_idle();
        workers.submit_body(request);
        wait_idle();
        assert_eq!(backend.diff_git_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.delta_batch_calls.load(Ordering::SeqCst), 2);

        assert_eq!(workers.try_recv_body().unwrap().width, 100);
        let batches = [(); 2].map(|()| {
            let response = workers.try_recv_body().unwrap();
            assert_eq!(response.width, 140);
            let BodyOutcome::BatchReady(entries) = response.outcome else {
                panic!("expected batch at the new width");
            };
            entries
        });
        assert!(Arc::ptr_eq(&batches[0], &batches[1]));
    }

    #[test]
    fn diff_draw_shows_loading_before_restoring_the_scroll_offset() {
        use ratatui::{Terminal, backend::TestBackend};

        let (_tmp, mut app) = fake_app(FakeBackend::new(":git"), &[]);
        app.header = Arc::new(Text::raw("change header"));
        app.diff_body = DiffBody::LoadingDiff;
        app.diff_scroll = 20;

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
        let loading = terminal.backend().to_string();
        assert!(loading.contains("change header"), "{loading}");
        assert!(loading.contains("loading diff…"), "{loading}");
        assert_eq!(app.diff_scroll, 20);

        let mut lines = vec![Line::raw(""); 20];
        lines[19] = Line::styled("resumed diff line", Style::default().fg(Color::Green));
        app.diff_body = DiffBody::Ready(Arc::new(Text::from(lines)));
        terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
        let ready = terminal.backend().to_string();
        assert!(ready.contains("resumed diff line"), "{ready}");
        assert!(!ready.contains("change header"), "{ready}");
        assert!(!ready.contains("loading diff…"), "{ready}");
    }

    #[test]
    fn batch_delta_keeps_cursor_visual_range_and_quit_live_while_blocked_and_counts_calls() {
        use std::sync::atomic::Ordering;

        let gate = Gate::new();
        let mut backend_inner = Arc::try_unwrap(FakeBackend::new(":git")).ok().unwrap();
        backend_inner.delta_gate = Some(Arc::clone(&gate));
        let backend = Arc::new(backend_inner);
        let (files, git_patch, per_file) = sample_multi_file_commit();
        let cid = "1111111111111111111111111111111111111111";
        backend.add_commit(cid, files, "Commit header: multi\n", &git_patch, per_file);

        let (_tmp, mut app) = fake_app(Arc::clone(&backend), &[(cid, "multi")]);
        app.diff_batch_config = Some(DeltaBatchConfig::delta_v1());

        // Step 1: metadata loads quickly; then body worker blocks inside delta.
        app.sync_diff(100);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.files.is_empty() {
            assert!(Instant::now() < deadline, "metadata never arrived");
            app.sync_diff(100);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(app.files.len(), 4);
        // Body worker should now have entered the blocked delta gate.
        gate.wait_entered(1);
        assert!(app.is_diff_loading());

        // Step 2: while delta is STILL blocked, focus files pane, press V,
        // extend range with j/j/j, shrink with k, and verify live UI state!
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Files);
        app.handle_key(key(KeyCode::Char('V')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.file_selected, 3);
        assert_eq!(
            app.file_selection
                .selected(app.file_selected, app.files.len()),
            vec![0, 1, 2, 3]
        );
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.file_selected, 2);
        assert_eq!(
            app.file_selection
                .selected(app.file_selected, app.files.len()),
            vec![0, 1, 2]
        );
        // Non-blocking sync_diff during rapid moves must preserve header,
        // show loading state (not stale file 0 diff), and NOT spawn extra batches.
        app.sync_diff(100);
        let loading_text = app.diff_plain_text();
        assert!(
            loading_text.contains("Commit header: multi"),
            "{loading_text}"
        );
        assert!(loading_text.contains("loading diff…"), "{loading_text}");
        assert!(!loading_text.contains("new_alpha"), "{loading_text}");
        assert_eq!(backend.diff_git_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.delta_batch_calls.load(Ordering::SeqCst), 1);

        // Step 3: release delta gate — completion must display file 2 (selected at completion)!
        gate.release_all();
        app.sync_diff_blocking(100);
        let text2 = app.diff_plain_text();
        assert!(text2.contains("DELTA[w=100]"), "{text2}");
        assert!(text2.contains("こんにちは"), "{text2}");
        assert!(!text2.contains("new_alpha"), "{text2}");
        assert_eq!(backend.diff_git_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.delta_batch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.diff_file_calls.load(Ordering::SeqCst), 0);

        // Step 4: first and repeat visits to ALL 4 files at width 100 are cache hits!
        for idx in [0, 1, 3, 2, 0, 3, 1] {
            app.file_selected = idx;
            app.sync_diff(100);
            assert!(
                !app.is_diff_loading(),
                "expected warm cache hit for idx {idx}"
            );
        }
        assert_eq!(backend.diff_git_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.delta_batch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.diff_file_calls.load(Ordering::SeqCst), 0);

        // Step 5: resize to 140 reuses raw patch (diff_git_calls stays 1!) and runs delta ONCE!
        app.file_selected = 0;
        app.sync_diff_blocking(140);
        let resized = app.diff_plain_text();
        assert!(resized.contains("DELTA[w=140]"), "{resized}");
        assert!(resized.contains("new_alpha"), "{resized}");
        assert_eq!(backend.diff_git_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.delta_batch_calls.load(Ordering::SeqCst), 2);
        assert_eq!(backend.diff_file_calls.load(Ordering::SeqCst), 0);

        // Resizing back to 100 hits Tier 2 rendered cache directly.
        app.sync_diff(100);
        assert!(!app.is_diff_loading());
        assert!(app.diff_plain_text().contains("DELTA[w=100]"));
        assert_eq!(backend.delta_batch_calls.load(Ordering::SeqCst), 2);

        // Step 6: quit input works immediately even while blocked on a new width.
        let gate2 = Gate::new();
        let mut b2 = Arc::try_unwrap(FakeBackend::new(":git")).ok().unwrap();
        b2.delta_gate = Some(Arc::clone(&gate2));
        let b2 = Arc::new(b2);
        let (f2, p2, pf2) = sample_multi_file_commit();
        b2.add_commit(cid, f2, "Commit header\n", &p2, pf2);
        let (_tmp2, mut app2) = fake_app(Arc::clone(&b2), &[(cid, "multi")]);
        app2.diff_batch_config = Some(DeltaBatchConfig::delta_v1());
        app2.sync_diff(80);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app2.files.is_empty() {
            assert!(Instant::now() < deadline);
            app2.sync_diff(80);
            thread::sleep(Duration::from_millis(1));
        }
        gate2.wait_entered(1);
        app2.handle_key(key(KeyCode::Char('q')));
        assert!(app2.should_quit, "quit must not wait for blocked delta");
        drop(app2);
        gate2.release_all();
    }

    #[test]
    fn slow_filter_coalesces_obsolete_moves_rejects_stale_results_and_caches_success() {
        use std::sync::atomic::Ordering;

        let gate = Gate::new();
        let mut backend_inner = Arc::try_unwrap(FakeBackend::new(":git")).ok().unwrap();
        backend_inner.filter_gate = Some(Arc::clone(&gate));
        let backend = Arc::new(backend_inner);
        let (files, git_patch, per_file) = sample_multi_file_commit();
        let cid = "2222222222222222222222222222222222222222";
        backend.add_commit(cid, files, "Filter header\n", &git_patch, per_file);

        let (_tmp, mut app) = fake_app(Arc::clone(&backend), &[(cid, "filter-test")]);
        // Setting SHIKIGAMI_DIFF_FILTER keeps per-file async path even when batch is configured.
        app.diff_batch_config = Some(DeltaBatchConfig::delta_v1());
        app.diff_filter_cmd = Some("fake-filter".to_string());

        app.sync_diff(90);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.files.is_empty() {
            assert!(Instant::now() < deadline);
            app.sync_diff(90);
            thread::sleep(Duration::from_millis(1));
        }
        // Worker enters filter for file 0 (alpha.rs) and blocks.
        gate.wait_entered(1);
        assert_eq!(backend.diff_git_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.delta_batch_calls.load(Ordering::SeqCst), 0);

        // Rapidly jump 0 -> 1 -> 2 -> 3 while file 0 is still blocked.
        app.handle_key(key(KeyCode::Tab));
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Char('j')));
            app.sync_diff(90);
        }
        assert_eq!(app.file_selected, 3);
        assert!(app.diff_plain_text().contains("loading diff…"));

        // Release file 0: stale result must NOT be displayed under file 3!
        gate.release_next();
        // Wait until worker enters filter for coalesced file 3 (skipping files 1 & 2!).
        gate.wait_entered(2);
        app.poll_diff();
        let still_loading = app.diff_plain_text();
        assert!(
            !still_loading.contains("RAW_ALPHA"),
            "stale file 0 diff leaked onto file 3: {still_loading}"
        );
        assert!(still_loading.contains("loading diff…"), "{still_loading}");

        // Release file 3 -> file 3 displays cleanly!
        gate.release_all();
        app.sync_diff_blocking(90);
        let text3 = app.diff_plain_text();
        assert!(text3.contains("FILTER[w=90]"), "{text3}");
        assert!(text3.contains("RAW_MODE"), "{text3}");
        // Only file 0 (in-flight) and file 3 (latest) ran — files 1 & 2 were coalesced!
        assert_eq!(backend.filter_calls.load(Ordering::SeqCst), 2);
        assert_eq!(backend.diff_file_calls.load(Ordering::SeqCst), 2);

        // Revisiting file 0 and file 3 at width 90 are cache hits (0 new filter/jj calls).
        app.file_selected = 0;
        app.sync_diff(90);
        assert!(!app.is_diff_loading());
        assert!(app.diff_plain_text().contains("RAW_ALPHA"));
        app.file_selected = 3;
        app.sync_diff(90);
        assert!(!app.is_diff_loading());
        assert!(app.diff_plain_text().contains("RAW_MODE"));
        assert_eq!(backend.filter_calls.load(Ordering::SeqCst), 2);
        assert_eq!(backend.diff_file_calls.load(Ordering::SeqCst), 2);

        // Resize to 120 reuses cached raw per-file diff (0 new jj calls) and runs filter once.
        app.sync_diff_blocking(120);
        assert!(app.diff_plain_text().contains("FILTER[w=120]"));
        assert_eq!(backend.diff_file_calls.load(Ordering::SeqCst), 2);
        assert_eq!(backend.filter_calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn non_git_formatter_and_corrupt_delta_fall_back_cleanly_without_poisoning_cache() {
        use std::sync::atomic::Ordering;

        // Case 1: ui.diff-formatter is :color-words -> batch disabled, per-file cached.
        let backend1 = FakeBackend::new(":color-words");
        let (files, git_patch, per_file) = sample_multi_file_commit();
        let cid = "3333333333333333333333333333333333333333";
        backend1.add_commit(cid, files, "Header\n", &git_patch, per_file);
        let (_tmp1, mut app1) = fake_app(Arc::clone(&backend1), &[(cid, "non-git")]);
        app1.diff_batch_config = Some(DeltaBatchConfig::delta_v1());
        app1.sync_diff_blocking(80);
        assert!(app1.diff_plain_text().contains("RAW_ALPHA"));
        assert_eq!(backend1.diff_git_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend1.delta_batch_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend1.diff_file_calls.load(Ordering::SeqCst), 1);
        // Revisit is cached.
        app1.file_selected = 1;
        app1.sync_diff_blocking(80);
        assert!(app1.diff_plain_text().contains("RAW_RENAME"));
        app1.file_selected = 0;
        app1.sync_diff(80);
        assert!(!app1.is_diff_loading());
        assert_eq!(backend1.diff_file_calls.load(Ordering::SeqCst), 2);

        // Case 2: delta outputs stray/corrupt markers -> falls back without poisoning cache!
        let backend2 = FakeBackend::new(":git");
        backend2.corrupt_delta.store(true, Ordering::SeqCst);
        let (files2, git_patch2, per_file2) = sample_multi_file_commit();
        backend2.add_commit(cid, files2, "Header\n", &git_patch2, per_file2);
        let (_tmp2, mut app2) = fake_app(Arc::clone(&backend2), &[(cid, "corrupt-delta")]);
        app2.diff_batch_config = Some(DeltaBatchConfig::delta_v1());
        app2.sync_diff_blocking(80);
        let fallback_text = app2.diff_plain_text();
        assert!(fallback_text.contains("RAW_ALPHA"), "{fallback_text}");
        assert!(
            !fallback_text.contains("STRAY_OUTSIDE_MARKERS"),
            "{fallback_text}"
        );
        assert_eq!(backend2.delta_batch_calls.load(Ordering::SeqCst), 1);
        // Switching to file 1 stays on fallback for this commit (does not retry broken delta).
        app2.file_selected = 1;
        app2.sync_diff_blocking(80);
        assert!(app2.diff_plain_text().contains("RAW_RENAME"));
        assert_eq!(backend2.delta_batch_calls.load(Ordering::SeqCst), 1);

        // A fresh batch attempt after repairing delta must fetch the raw patch again.
        backend2.corrupt_delta.store(false, Ordering::SeqCst);
        app2.batch_fallback_commits.clear();
        app2.sync_diff_blocking(80);
        assert_eq!(backend2.diff_git_calls.load(Ordering::SeqCst), 2);
        assert!(app2.diff_plain_text().contains("DELTA[w=80]"));
    }

    #[test]
    fn real_jj_and_delta_batch_handles_special_paths_hunkless_changes_and_resize() {
        if !jj_available() {
            report_skip();
            return;
        }
        let delta_ok = Command::new("delta")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !delta_ok {
            eprintln!("skipping: `delta` is not on PATH");
            return;
        }

        let (tmp, mut app) = repo_or_skip!();
        let dir = tmp.path();
        run(
            dir,
            &["config", "set", "--repo", "ui.diff-formatter", ":git"],
        );
        std::fs::write(dir.join("old space.txt"), "hello\n").unwrap();
        std::fs::write(dir.join("mode.sh"), "#!/bin/sh\necho hi\n").unwrap();
        run(dir, &["new", "-m", "batch delta showcase"]);
        std::fs::rename(dir.join("old space.txt"), dir.join("new space.txt")).unwrap();
        std::fs::write(dir.join("日本語 file.txt"), "こんにちは\n世界\n").unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3, 255, 0, 4]).unwrap();
        std::fs::write(dir.join("empty_new.txt"), "").unwrap();
        run(dir, &["file", "chmod", "x", "mode.sh"]);

        app.diff_filter_cmd = None;
        app.diff_batch_config = Some(DeltaBatchConfig::delta_v1());
        app.reload().unwrap();
        while !app.selected_change().unwrap().is_working_copy {
            app.handle_key(key(KeyCode::Char('k')));
        }
        assert_eq!(
            app.selected_change().unwrap().description,
            "batch delta showcase"
        );
        app.sync_diff_blocking(100);
        assert_eq!(app.files.len(), 5, "{:?}", app.files);

        // Every file must be served cleanly from the prepared batch without fallback.
        assert!(app.batch_fallback_commits.is_empty());
        assert!(app.batch_disabled_generation.is_none());
        for i in 0..app.files.len() {
            app.file_selected = i;
            app.sync_diff(100);
            assert!(
                !app.is_diff_loading(),
                "expected immediate batch cache hit for {:?}",
                app.files[i]
            );
            let text = app.diff_plain_text();
            assert!(
                text.contains("batch delta showcase"),
                "header missing for {:?}: {text}",
                app.files[i]
            );
            assert!(
                text.contains(&app.files[i].destination_path),
                "expected destination path in delta output for {:?}: {text}",
                app.files[i]
            );
        }

        // Resize to 130 re-runs delta using cached raw patch and warms all files again.
        app.sync_diff_blocking(130);
        assert!(app.batch_fallback_commits.is_empty());
        for i in 0..app.files.len() {
            app.file_selected = i;
            app.sync_diff(130);
            assert!(!app.is_diff_loading());
        }
    }
}
