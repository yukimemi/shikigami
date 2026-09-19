//! TUI の状態機械。
//!
//! 描画 (`ui`) と端末の面倒 (`tui`) からは独立させてあり、キー入力 →
//! 状態遷移 → jj 呼び出しまでをここで完結させる。おかげで、端末を開かずに
//! 「このキーで本当にこの jj コマンドが飛ぶか」をテストできる。

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Text;

use crate::jj::{Change, DiffFile, Jj, RebaseMode, Row};

/// 一度に動かす行数 (Ctrl-d / Ctrl-u)。
const PAGE: usize = 10;

/// どちらの pane にキーが効くか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Log,
    /// 選択中 change のファイル一覧 (lazygit の files pane 相当)。
    Files,
    Diff,
}

/// 入力待ちの用途。確定したときに何をするかを持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    /// `jj describe -r <rev> -m <input>`
    Describe { rev: String },
    /// `jj new <rev> -m <input>` (input 空なら `-m` なし)
    NewChild { rev: String },
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
            InputAction::Describe { rev } | InputAction::NewChild { rev } => Some(rev.as_str()),
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
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Info,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Error,
        }
    }
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
    /// diff pane の中身 (ANSI を解決済みの ratatui Text)。
    pub diff: Text<'static>,
    /// 選択中 change で変更されたファイル一覧 (lazygit の files pane 相当)。
    pub files: Vec<DiffFile>,
    /// `files` の index。ここで選んだ 1 ファイルだけが `diff` に出る。
    pub file_selected: usize,
    /// `files` がどの commit のものか。selection が動いたかの判定に使う。
    files_of: Option<String>,
    /// `jj show -r rev --no-patch` の結果 (ANSI 解決済み)。ファイル切り替え
    /// だけなら変わらないので、change ごとに 1 回だけ取って使い回す。
    header: Text<'static>,
    /// `header` がどの commit のものか。
    header_of: Option<String>,
    /// `diff` が (どの commit, どのファイル) のものか。
    /// `file` 側が `None` なら change 全体の diff。
    diff_rendered: Option<(String, Option<String>)>,
    pub diff_scroll: u16,
    pub working_copy: String,
    /// `SHIKIGAMI_AI_CMD` の値。起動時に一度だけ読む (`App::new`)。
    /// `None` なら AI 生成 (Ctrl-g) は使えない。
    pub ai_cmd: Option<String>,
    pub should_quit: bool,
}

impl App {
    pub fn new(jj: Jj, revset: Option<String>) -> Result<Self> {
        let mut app = Self {
            jj,
            rows: Vec::new(),
            selected: 0,
            revset,
            focus: Focus::Log,
            mode: Mode::Normal,
            status: Status::info("? for help"),
            marked: None,
            rebase_mode: RebaseMode::Revision,
            diff: Text::default(),
            files: Vec::new(),
            file_selected: 0,
            files_of: None,
            header: Text::default(),
            header_of: None,
            diff_rendered: None,
            diff_scroll: 0,
            working_copy: String::new(),
            ai_cmd: std::env::var(crate::ai::AI_CMD_ENV)
                .ok()
                .filter(|cmd| !cmd.trim().is_empty()),
            should_quit: false,
        };
        app.reload()?;
        Ok(app)
    }

    /// log を読み直す。選択は change id で追いかけるので、squash や
    /// rebase で行数が変わってもカーソルが飛ばない。
    pub fn reload(&mut self) -> Result<()> {
        let keep = self.selected_change().map(|c| c.id.clone());
        self.rows = self.jj.log(self.revset.as_deref())?;
        self.selected = keep
            .and_then(|id| self.index_of(&id))
            .or_else(|| self.first_change_index())
            .unwrap_or(0);
        self.working_copy = self.jj.status_line().unwrap_or_default();
        // 消えた change を mark したままにすると、以降の squash/rebase が
        // 毎回 jj 側のエラーになる。log から消えたら外す。
        if let Some(marked) = &self.marked
            && self.index_of(&marked.id).is_none()
        {
            self.marked = None;
        }
        self.files_of = None;
        self.header_of = None;
        Ok(())
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

    /// 選択が変わっていたら diff を取り直す。描画前に呼ぶ。
    ///
    /// 遅延させる理由: j/k で連続移動しているあいだに 1 行ごとに
    /// `jj diff` を起動すると、キー入力より diff の方が遅くなる。
    /// 描画直前に「今の選択の分だけ」1 回取る。
    ///
    /// change が変わった場合はまずファイル一覧 (`jj diff --summary`) と
    /// header (`jj show --no-patch`) を取り直し、files pane の選択を
    /// 先頭に戻す。files pane 内で選択ファイルだけが変わった場合は
    /// header を使い回し、`jj diff` だけ叩く — 起動する `jj` プロセスを
    /// 1 個に抑える (Windows は `CreateProcess` が重く、j/k のたびに
    /// `jj show` + `jj diff` の 2 プロセスを立てると files pane の
    /// 連打がもたつく)。
    pub fn sync_diff(&mut self) {
        let Some(change) = self.selected_change() else {
            self.files = Vec::new();
            self.file_selected = 0;
            self.files_of = None;
            self.header = Text::default();
            self.header_of = None;
            self.diff = Text::default();
            self.diff_rendered = None;
            return;
        };
        let rev = change.id.clone();
        let commit_id = change.commit_id.clone();
        if self.files_of.as_deref() != Some(commit_id.as_str()) {
            self.files = self.jj.diff_summary(&rev).unwrap_or_default();
            self.file_selected = 0;
            self.files_of = Some(commit_id.clone());
        }
        if self.header_of.as_deref() != Some(commit_id.as_str()) {
            self.header = match self.render_header(&rev) {
                Ok(header) => header,
                Err(err) => Text::from(format!("{err}")),
            };
            self.header_of = Some(commit_id.clone());
        }
        let path = self.selected_file().map(|f| f.path.clone());
        let key = (commit_id, path.clone());
        if self.diff_rendered.as_ref() == Some(&key) {
            return;
        }
        let text = match self.render_diff(self.header.clone(), &rev, path.as_deref()) {
            Ok(text) => text,
            Err(err) => Text::from(format!("{err}")),
        };
        self.diff = text;
        self.diff_rendered = Some(key);
        self.diff_scroll = 0;
    }

    /// diff pane のヘッダ部分 (`jj show --no-patch` 相当)。change が
    /// 変わったときだけ呼ぶ。
    fn render_header(&self, rev: &str) -> Result<Text<'static>> {
        use ansi_to_tui::IntoText;
        self.jj
            .show(rev)?
            .into_bytes()
            .into_text()
            .map_err(anyhow::Error::from)
    }

    /// `header` に選択中ファイル (`path`; `None` なら change 全体) の
    /// diff を続ける。
    fn render_diff(
        &self,
        mut header: Text<'static>,
        rev: &str,
        path: Option<&str>,
    ) -> Result<Text<'static>> {
        use ansi_to_tui::IntoText;
        let diff = self.jj.diff(rev, path)?;
        if diff.trim().is_empty() {
            header.extend(Text::from("(no changes)"));
        } else {
            header.extend(diff.into_bytes().into_text().map_err(anyhow::Error::from)?);
        }
        Ok(header)
    }

    pub fn scroll_diff(&mut self, delta: isize) {
        let next = self.diff_scroll as isize + delta;
        let max = self.diff.lines.len().saturating_sub(1) as isize;
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
                let text = err
                    .to_string()
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" | ");
                self.status = Status::error(format!("{label} failed: {text}"));
            }
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
    }

    fn handle_input(&mut self, key: KeyEvent, mut input: Input) {
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
    /// 対象 rev (Describe/NewChild が持つもの) の diff を `ai_cmd` に
    /// 渡し、返ってきたメッセージで入力値を丸ごと置き換える。diff を
    /// 持たない入力 (bookmark 名や revset) では意味がないので何もしない。
    fn fill_with_ai(&mut self, mut input: Input) {
        let Some(rev) = input.action.ai_rev().map(str::to_string) else {
            self.status = Status::error("ai: not available for this prompt");
            self.mode = Mode::Input(input);
            return;
        };
        match self.generate_ai_message(&rev) {
            Ok(message) => {
                input.value = message;
                self.status = Status::info("ai: message generated");
            }
            Err(err) => self.status = Status::error(format!("ai: {err}")),
        }
        self.mode = Mode::Input(input);
    }

    fn generate_ai_message(&self, rev: &str) -> Result<String> {
        let cmd = self
            .ai_cmd
            .as_deref()
            .with_context(|| format!("{} is not set", crate::ai::AI_CMD_ENV))?;
        let diff = self.jj.diff_plain(rev)?;
        crate::ai::generate_message(cmd, &diff)
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
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Log => Focus::Files,
                    Focus::Files => Focus::Diff,
                    Focus::Diff => Focus::Log,
                }
            }
            KeyCode::Char('d') if ctrl => self.step(PAGE as isize),
            KeyCode::Char('u') if ctrl => self.step(-(PAGE as isize)),
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
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
            KeyCode::Char('r') if ctrl => match self.reload() {
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
            KeyCode::Enter => self.edit_selected(),
            KeyCode::Char('n') => self.prompt_new(),
            KeyCode::Char('e') => self.prompt_describe(),
            KeyCode::Char('s') => self.confirm_squash(),
            KeyCode::Char('b') => self.prompt_bookmark(),
            KeyCode::Char('a') => self.confirm_abandon(),
            KeyCode::Char('x') => self.confirm_rebase(),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::repo_or_skip;
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
        app.sync_diff();
        let text = app
            .diff
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
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
        assert!((app.diff_scroll as usize) < app.diff.lines.len());
        // focus が diff のあいだ、log の選択は動かない。
        let selected = app.selected_change().unwrap().id.clone();
        app.handle_key(ctrl('d'));
        assert_eq!(app.selected_change().unwrap().id, selected);
    }

    #[test]
    fn files_pane_lists_every_changed_file_and_diff_follows_the_selection() {
        let (_tmp, mut app) = repo_or_skip!();
        // working copy (third change) に 2 ファイル追加して、files pane に
        // 複数行出ることと、選択したファイルだけが diff pane に出ることを
        // 確認する。
        std::fs::write(app.jj.root().join("x.txt"), "x\n").unwrap();
        std::fs::write(app.jj.root().join("y.txt"), "y\n").unwrap();
        app.reload().unwrap();
        app.sync_diff();

        let mut paths: Vec<&str> = app.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["x.txt", "y.txt"], "{:?}", app.files);

        let text_of = |app: &App| -> String {
            app.diff
                .lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // 先頭ファイルだけが diff pane に出て、もう一方は出ない。
        let first_path = app.files[0].path.clone();
        let second_path = app.files[1].path.clone();
        let first_text = text_of(&app);
        assert!(first_text.contains(&format!("diff --git a/{first_path}")));
        assert!(!first_text.contains(&format!("diff --git a/{second_path}")));

        // files pane に移って次のファイルへ動かすと、diff pane が切り替わる。
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Files);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.file_selected, 1);
        app.sync_diff();
        let second_text = text_of(&app);
        assert!(second_text.contains(&format!("diff --git a/{second_path}")));
        assert!(!second_text.contains(&format!("diff --git a/{first_path}")));
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

    #[test]
    fn ai_ctrl_g_fills_the_describe_prompt_from_the_diff() {
        let (_tmp, mut app) = repo_or_skip!();
        app.ai_cmd = Some(echo_message_cmd("feat: ai generated message"));
        while app.selected_change().unwrap().description != "second change" {
            app.handle_key(key(KeyCode::Char('j')));
        }
        app.handle_key(key(KeyCode::Char('e')));
        app.handle_key(ctrl('g'));
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
}
