//! jj CLI との境界。
//!
//! shikigami は jj のライブラリ (`jj-lib`) を使わず、`jj` バイナリを
//! 叩いて stdout をパースする。理由は 2 つ:
//!
//! 1. `jj-lib` は semver 保証がなく、jj 本体の更新でビルドが壊れる。CLI の
//!    template 言語と引数は jj の公開インタフェースで、はるかに安定している。
//! 2. CLI 経由なら user の `~/.config/jj/config.toml` (revsets.log,
//!    immutable_heads, diff formatter, 色) が自動的に効く。lib 直叩きだと
//!    その再実装が必要になる。

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// レコード先頭マーカー (ASCII record separator)。
///
/// `jj log` は graph 描画を stdout に混ぜて出す。template 出力の直前に
/// この 1 文字を置いておくと、「行のうちマーカーより前が graph、後が
/// データ」と決まるので、graph の枝 (`│ ○`, `├─╮`) を自前で描かずに
/// jj のものをそのまま使える。マーカーを含まない行は枝だけの行。
const RECORD: char = '\x1e';
/// フィールド区切り (ASCII unit separator)。description に tab や `,` が
/// 入っていても壊れない文字を選ぶ。
const FIELD: char = '\x1f';

/// `jj log` に渡す template。フィールド順は [`Change`] のパース順と一対一。
const LOG_TEMPLATE: &str = concat!(
    r#""\x1e""#,
    r#" ++ change_id ++ "\x1f""#,
    r#" ++ change_id.shortest(8) ++ "\x1f""#,
    r#" ++ commit_id.shortest(8) ++ "\x1f""#,
    r#" ++ commit_id ++ "\x1f""#,
    r#" ++ if(current_working_copy, "1", "0") ++ "\x1f""#,
    r#" ++ if(empty, "1", "0") ++ "\x1f""#,
    r#" ++ if(conflict, "1", "0") ++ "\x1f""#,
    r#" ++ if(immutable, "1", "0") ++ "\x1f""#,
    r#" ++ author.name() ++ "\x1f""#,
    r#" ++ author.timestamp().local().format("%Y-%m-%d %H:%M") ++ "\x1f""#,
    r#" ++ bookmarks.map(|b| b.name()).join(",") ++ "\x1f""#,
    r#" ++ description.first_line() ++ "\n""#,
);

/// `jj log` の 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// change 1 件。`graph` はその行の graph 部分 (`"@  "`, `"│ ○  "` 等)。
    Change { graph: String, change: Change },
    /// 枝だけの行 (`"├─╮"` 等)。選択対象にはならないが、描画には必要。
    Connector(String),
}

/// files pane の表示文字列と、jj から取得した曖昧でない実パス。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffFile {
    /// jj のステータス文字 (`M`/`A`/`D`/`R`/`C`)。
    pub status: char,
    /// 表示専用。異なるリネームが同じ圧縮表記になる場合もある。
    pub path: String,
    /// リポジトリルート相対の実パス。旧キャッシュからの推測復元はしない。
    pub source_path: String,
    pub destination_path: String,
}

impl DiffFile {
    pub fn target_path(&self) -> String {
        self.destination_path.clone()
    }

    /// diff の対象となる実パス。削除側も含めてリネームを表示する。
    pub fn diff_paths(&self) -> Vec<String> {
        if matches!(self.status, 'R' | 'C') {
            vec![self.source_path.clone(), self.destination_path.clone()]
        } else {
            vec![self.destination_path.clone()]
        }
    }

    /// 新パスだけでは旧パスが復元されないため、リネームの両側を渡す。
    pub fn restore_paths(&self) -> Vec<String> {
        self.diff_paths()
    }
}

// RepoPath の JSON は OS によらず `/` 区切りのリポジトリ相対パス。
// 表示文字列には手を加えず、引用符・改行等も JSON で安全に区切る。
const DIFF_SUMMARY_TEMPLATE: &str = concat!(
    r#"'{"status":' ++ json(status_char) ++ ',"path":' ++ json(display_diff_path)"#,
    r#" ++ ',"source_path":' ++ json(source.path())"#,
    r#" ++ ',"destination_path":' ++ json(target.path()) ++ "}\n""#,
);

const FILE_LIST_JSON_TEMPLATE: &str = r#"json(path) ++ "\n""#;
const UNTRACK_ALIAS: &str = "__shikigami_untrack_targets";

/// 2 語で構成される jj サブコマンド。エラー要約時に第 2 語まで残す。
const NESTED_SUBCOMMANDS: &[&str] = &[
    "bookmark",
    "config",
    "debug",
    "file",
    "git",
    "op",
    "operation",
    "sparse",
    "util",
    "workspace",
];

fn normalize_repo_path(path: &str) -> String {
    // Windows の jj だけが `\` を区切りとして出す。Unix では
    // ファイル名の一部なのでそのまま残す。
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    }
}

/// リポジトリ相対パスを fileset のリテラルとして渡す。
fn literal_fileset(path: &str, kind: &str) -> String {
    let path = normalize_repo_path(path);
    let mut escaped = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '\\' => escaped.push_str(r"\\"),
            '"' => escaped.push_str(r#"\""#),
            '\t' => escaped.push_str(r"\t"),
            '\r' => escaped.push_str(r"\r"),
            '\n' => escaped.push_str(r"\n"),
            '\0' => escaped.push_str(r"\0"),
            c if (c as u32) < 0x20 || c == '\u{007F}' => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\x{:02x}", c as u32);
            }
            c => escaped.push(c),
        }
    }
    format!("{kind}:\"{escaped}\"")
}

/// TOML v1.0 の基本文字列 (`"..."`) として安全にシリアライズする。
fn toml_basic_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            '\u{0008}' => out.push_str(r"\b"),
            '\t' => out.push_str(r"\t"),
            '\n' => out.push_str(r"\n"),
            '\u{000C}' => out.push_str(r"\f"),
            '\r' => out.push_str(r"\r"),
            c if (c as u32) < 0x20 || c == '\u{007F}' => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 選択パス列を完全一致 (`root-file:`) の和集合 fileset 式に組み立てる。
fn fileset_union_expr(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| literal_fileset(path, "root-file"))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn fileset_alias_toml(alias: &str, expr: &str) -> String {
    format!("[fileset-aliases]\n{alias} = {}\n", toml_basic_string(expr))
}

/// 引数全体ではなくサブコマンド名だけを抜き出す。status bar にパス列や
/// テンプレートが溢れて OS エラー原因が隠れるのを防ぐ。
fn summarize_command(args: &[&str]) -> String {
    let mut parts = args.iter().copied().filter(|arg| !arg.starts_with('-'));
    match parts.next() {
        Some(first) if NESTED_SUBCOMMANDS.contains(&first) => match parts.next() {
            Some(second) => format!("{first} {second}"),
            None => first.to_string(),
        },
        Some(first) => first.to_string(),
        None => String::new(),
    }
}

fn write_untrack_config(
    root: &Path,
    temp_dir: &Path,
    contents: &str,
) -> Result<tempfile::NamedTempFile> {
    let temp_dir = temp_dir
        .canonicalize()
        .context("cannot resolve temporary directory")?;
    if temp_dir.starts_with(root) {
        bail!("temporary directory must be outside the working copy");
    }
    let mut config = tempfile::Builder::new()
        .prefix("shikigami-untrack-")
        .suffix(".toml")
        .tempfile_in(temp_dir)
        .context("cannot create temporary fileset config")?;
    config
        .write_all(contents.as_bytes())
        .context("cannot write temporary fileset config")?;
    Ok(config)
}

/// `jj file untrack` 失敗後の `@` 追跡一覧との照合結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrackProgress {
    /// エイリアス非依存の `jj file list -r @` で確認できた件数。
    Verified { untracked: usize, remaining: usize },
    /// 追跡一覧の照合に失敗したため、件数を推測せず不明とする。
    Unknown,
}

/// `jj file untrack` の失敗分類。起動前か起動後かを呼び出し側へ伝える。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UntrackFailure {
    /// `jj file untrack` の起動前に失敗 (0 件解除が確定)。
    NotStarted { detail: String },
    /// `jj file untrack` が起動後に非 0 終了。実状態は `progress` で照合する。
    AfterLaunch {
        detail: String,
        progress: UntrackProgress,
    },
}

impl UntrackFailure {
    pub fn detail(&self) -> &str {
        match self {
            UntrackFailure::NotStarted { detail } | UntrackFailure::AfterLaunch { detail, .. } => {
                detail
            }
        }
    }
}

impl fmt::Display for UntrackFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

/// 事前照合済みの `jj file untrack` バッチ。作業コピー外の一時設定ファイルを
/// 保持し、実行完了または中止時のドロップで片付ける。
pub struct UntrackBatch<'a> {
    jj: &'a Jj,
    config: tempfile::NamedTempFile,
    expected: BTreeSet<String>,
}

impl UntrackBatch<'_> {
    /// 確定済みの fileset エイリアスを 1 回の `jj file untrack` で実行する。
    pub fn execute(self) -> std::result::Result<String, UntrackFailure> {
        let UntrackBatch {
            jj,
            config,
            expected,
        } = self;
        let result = jj.exec_raw(
            WRITE_GLOBALS,
            Some(config.path()),
            &["file", "untrack", UNTRACK_ALIAS],
            OutputCapture::Full,
            false,
        );
        // 一時設定ファイルは `jj file untrack` 終了直後に片付け、失敗時の
        // 照合がエイリアスに一切依存しないことも明確にする。
        drop(config);
        match result {
            Ok(out) => Ok(out),
            Err(ExecError::Spawn { cmd_label, source }) => Err(UntrackFailure::NotStarted {
                detail: format!("failed to run `{cmd_label}`: {source:#}"),
            }),
            Err(ExecError::Failed {
                cmd_label,
                status,
                stderr,
            }) => {
                let detail = if stderr.is_empty() {
                    format!("`{cmd_label}` failed with {status}")
                } else {
                    stderr
                };
                let progress = jj.reconcile_untrack_progress(&expected);
                Err(UntrackFailure::AfterLaunch { detail, progress })
            }
        }
    }
}

/// log に出てくる 1 change。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// 完全な change id。jj へコマンドを投げるときは常にこれを使う
    /// (短縮 id は repo が育つと曖昧になりうる)。
    pub id: String,
    /// 表示用の短縮 change id。
    pub short_id: String,
    /// 表示用の短縮 commit id。
    pub commit_id: String,
    /// 完全な不変 commit id。バックグラウンド diff 取得やキャッシュキーに使う。
    pub full_commit_id: String,
    pub is_working_copy: bool,
    pub is_empty: bool,
    pub has_conflict: bool,
    /// immutable (`immutable_heads()` 配下)。書き換え系の操作は jj 側で
    /// 拒否されるので、UI では事前に警告を出すためだけに使う。
    pub is_immutable: bool,
    pub author: String,
    pub timestamp: String,
    pub bookmarks: Vec<String>,
    /// description の 1 行目。空なら空文字列 (UI 側で `(no description)`)。
    pub description: String,
}

impl Change {
    /// 一覧に出す 1 行分のラベル (graph 部分は含まない)。
    pub fn summary(&self) -> String {
        if self.description.is_empty() {
            "(no description set)".to_string()
        } else {
            self.description.clone()
        }
    }
}

/// rebase の対象指定モード。jj の `-r` / `-s` / `-b` に対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseMode {
    /// `-r`: その revision だけ。子孫は元の親に残る。
    Revision,
    /// `-s`: その revision と子孫すべて。
    Source,
    /// `-b`: destination から見た branch 全体。
    Branch,
}

impl RebaseMode {
    pub fn flag(self) -> &'static str {
        match self {
            RebaseMode::Revision => "-r",
            RebaseMode::Source => "-s",
            RebaseMode::Branch => "-b",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RebaseMode::Revision => "revision (-r)",
            RebaseMode::Source => "source+descendants (-s)",
            RebaseMode::Branch => "branch (-b)",
        }
    }

    /// `-r` → `-s` → `-b` → `-r` と循環させる (UI の切り替えキー用)。
    pub fn next(self) -> Self {
        match self {
            RebaseMode::Revision => RebaseMode::Source,
            RebaseMode::Source => RebaseMode::Branch,
            RebaseMode::Branch => RebaseMode::Revision,
        }
    }
}

enum OutputCapture {
    Full,
    DiffPane,
}

enum ExecError {
    Spawn {
        cmd_label: String,
        source: anyhow::Error,
    },
    Failed {
        cmd_label: String,
        status: std::process::ExitStatus,
        stderr: String,
    },
}

impl ExecError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            ExecError::Spawn { cmd_label, source } => {
                source.context(format!("failed to run `{cmd_label}`"))
            }
            ExecError::Failed {
                cmd_label,
                status,
                stderr,
            } => {
                if stderr.is_empty() {
                    anyhow::anyhow!("`{cmd_label}` failed with {status}")
                } else {
                    anyhow::anyhow!("{stderr}")
                }
            }
        }
    }
}

/// 1 つの jj repo に対する操作の入口。
#[derive(Debug, Clone)]
pub struct Jj {
    root: PathBuf,
}

impl Jj {
    /// `start` から親方向に `.jj` を探して repo を開く。
    ///
    /// `.git` は見ない: colocated repo なら `.jj` も在るので当たるし、
    /// git 専用 repo で当ててしまうと「jj repo でないのに TUI が開いて
    /// 全操作が失敗する」という最悪の挙動になる。
    pub fn discover(start: &Path) -> Result<Self> {
        let start = start
            .canonicalize()
            .with_context(|| format!("cannot resolve path: {}", start.display()))?;
        let mut cur: Option<&Path> = Some(&start);
        while let Some(dir) = cur {
            if dir.join(".jj").is_dir() {
                return Ok(Self {
                    root: dir.to_path_buf(),
                });
            }
            cur = dir.parent();
        }
        bail!(
            "no jj repo found in {} or any parent directory (run `jj git init --colocate` first)",
            start.display()
        )
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 読み取り系コマンド。`--color never` で機械可読な出力にする。
    fn read(&self, args: &[&str]) -> Result<String> {
        self.exec(READ_GLOBALS, args, OutputCapture::Full)
    }

    fn read_diff_pane(&self, args: &[&str]) -> Result<String> {
        self.exec(READ_GLOBALS, args, OutputCapture::DiffPane)
    }

    /// 書き換え系コマンド。
    ///
    /// `ui.editor` を存在しないプログラムに差し替えるのが肝。jj が
    /// editor を立ち上げるのは「description が未指定で必要になった」
    /// 場合だけだが、alternate screen 内で $EDITOR が起動すると画面が
    /// 壊れて復帰もできない。差し替えておけば、その経路に入った瞬間に
    /// エラーとして stderr に出るので、status bar に出して終われる。
    /// shikigami は description を要する操作では必ず `-m` / `-u` を渡す
    /// ので、この guard が発火するのは想定外のケースだけ。
    fn write(&self, args: &[&str]) -> Result<String> {
        self.exec_output(WRITE_GLOBALS, args, OutputCapture::Full, false)
    }

    fn exec(&self, globals: &[&str], args: &[&str], capture: OutputCapture) -> Result<String> {
        self.exec_output(globals, args, capture, false)
    }

    fn exec_output(
        &self,
        globals: &[&str],
        args: &[&str],
        capture: OutputCapture,
        include_stderr: bool,
    ) -> Result<String> {
        self.exec_raw(globals, None, args, capture, include_stderr)
            .map_err(ExecError::into_anyhow)
    }

    fn exec_with_config(
        &self,
        globals: &[&str],
        config_file: Option<&Path>,
        args: &[&str],
    ) -> Result<String> {
        self.exec_raw(globals, config_file, args, OutputCapture::Full, false)
            .map_err(ExecError::into_anyhow)
    }

    fn exec_raw(
        &self,
        globals: &[&str],
        config_file: Option<&Path>,
        args: &[&str],
        capture: OutputCapture,
        include_stderr: bool,
    ) -> std::result::Result<String, ExecError> {
        let summary = summarize_command(args);
        let cmd_label = if summary.is_empty() {
            "jj".to_string()
        } else {
            format!("jj {summary}")
        };
        let mut cmd = Command::new("jj");
        cmd.current_dir(&self.root);
        cmd.args(globals);
        if let Some(path) = config_file {
            cmd.arg("--config-file").arg(path);
        }
        cmd.args(args);
        // 認証プロンプトが TUI の入力を奪わないよう、標準入力を閉じる。
        let out = match capture {
            OutputCapture::DiffPane => crate::diff_filter::capture(&mut cmd, None),
            OutputCapture::Full => cmd
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(Into::into),
        }
        .map_err(|source| ExecError::Spawn {
            cmd_label: cmd_label.clone(),
            source,
        })?;
        if !out.status.success() {
            // jj のエラーは stderr に人間向けの Error/Hint で出る。
            // それをそのまま status bar に載せたいので、整形せず返す。
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(ExecError::Failed {
                cmd_label,
                status: out.status,
                stderr,
            });
        }
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        if include_stderr && !out.stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        Ok(text)
    }

    /// Remote summaries and warnings are written to stderr even on success.
    pub fn git_fetch(&self) -> Result<String> {
        self.exec_output(READ_GLOBALS, &["git", "fetch"], OutputCapture::Full, true)
    }

    /// Deliberately leave bookmark selection and creation policy to jj.
    pub fn git_push(&self) -> Result<String> {
        self.exec_output(READ_GLOBALS, &["git", "push"], OutputCapture::Full, true)
    }

    /// trunk bookmark 名。`trunk()` の指す change に付いたローカルと
    /// `@git` 以外のリモート bookmark 名がちょうど 1 つならその名前、照会失敗・なし・複数なら
    /// 意図しない bookmark を動かさないよう `main` に倒す。
    pub fn trunk_bookmark(&self) -> String {
        let out = self.read(&[
            "log",
            "--no-graph",
            "-r",
            "trunk()",
            "--template",
            concat!(
                r#"local_bookmarks.map(|b| b.name() ++ "\n").join("") ++ "#,
                r#"remote_bookmarks.map(|b| if(b.remote() != "git", b.name() ++ "\n")).join("")"#,
            ),
        ]);
        let names: BTreeSet<String> = out
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect();
        let mut it = names.into_iter();
        match (it.next(), it.next()) {
            (Some(name), None) => name,
            _ => "main".to_string(),
        }
    }

    /// `jj bookmark move <trunk> --to @-` (alias `done`)。移動先は
    /// 選択行ではなく常に作業コピーの親。後退は jj の拒否に任せる。
    pub fn bookmark_done(&self) -> Result<String> {
        let name = self.trunk_bookmark();
        let out = self
            .exec_output(
                WRITE_GLOBALS,
                &["bookmark", "move", &name, "--to", "@-"],
                OutputCapture::Full,
                true,
            )
            .map_err(|err| anyhow::anyhow!("jj bookmark move {name} --to @-: {err}"))?;
        let detail = out.trim();
        Ok(if detail.is_empty() {
            format!("moved {name} to @-")
        } else {
            format!("{name} -> @-: {detail}")
        })
    }

    /// 起動時の環境チェック用。`jj --version` の 1 行目。
    pub fn version() -> Result<String> {
        let out = Command::new("jj").arg("--version").output().context(
            "`jj` not found on PATH — shikigami drives the jj CLI and cannot run without it",
        )?;
        if !out.status.success() {
            bail!("`jj --version` failed with {}", out.status);
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// log を取得する。`revset` が `None` なら jj の `revsets.log` 設定
    /// (既定 `present(@) | ancestors(immutable_heads().., 2) | trunk()`)
    /// がそのまま効く。
    pub fn log(&self, revset: Option<&str>) -> Result<Vec<Row>> {
        let mut args = vec!["log", "--template", LOG_TEMPLATE];
        if let Some(revset) = revset {
            args.push("-r");
            args.push(revset);
        }
        let out = self.read(&args)?;
        Ok(parse_log(&out))
    }

    /// 選択中 change の diff。`--color always` なので ANSI 付き。
    /// フォーマットは固定しない — jj の `ui.diff-formatter` (`:git` /
    /// `:color-words` / difftastic のような外部ツール) にそのまま従う。
    /// これにより shikigami は「シェルで見えるのと同じ見た目」という
    /// README の設計方針を、diff-formatter を自前で選んだ場合にも保つ。
    /// `paths` の実パスだけに絞る。空なら change 全体を表示する。
    pub fn diff(&self, rev: &str, paths: &[String]) -> Result<String> {
        let filesets: Vec<String> = paths
            .iter()
            .map(|path| literal_fileset(path, "root-file"))
            .collect();
        let mut args = vec!["diff", "-r", rev, "--"];
        args.extend(filesets.iter().map(String::as_str));
        self.exec(
            &["--no-pager", "--color", "always"],
            &args,
            OutputCapture::DiffPane,
        )
    }

    /// AI コマンドに渡す change 全体の Git パッチ (ANSI なし)。
    /// diff pane の出力上限は適用しない。
    pub fn diff_plain(&self, rev: &str) -> Result<String> {
        self.read(&["diff", "-r", rev, "--git"])
    }

    /// バッチ整形用の不変コミット全体 Git パッチ (`--color never --git`)。
    pub fn diff_git_full(&self, full_commit_id: &str) -> Result<String> {
        self.read_diff_pane(&["diff", "-r", full_commit_id, "--git"])
    }

    /// 現在のリポジトリで有効な `ui.diff-formatter` 設定値を取得する。
    pub fn diff_formatter(&self) -> Result<String> {
        let out = self.read_diff_pane(&["config", "get", "ui.diff-formatter"])?;
        Ok(out.trim().to_string())
    }

    /// 選択中 change で変更されたファイルの表示名と実パスの一覧。
    /// lazygit のようにファイル単位で diff を選べるようにするための入口。
    pub fn diff_summary(&self, rev: &str) -> Result<Vec<DiffFile>> {
        self.diff_summary_with_globals(rev, READ_GLOBALS)
    }

    fn diff_summary_with_globals(&self, rev: &str, globals: &[&str]) -> Result<Vec<DiffFile>> {
        let out = self.exec(
            globals,
            &["diff", "-r", rev, "--template", DIFF_SUMMARY_TEMPLATE],
            OutputCapture::DiffPane,
        )?;
        parse_diff_summary(&out)
    }

    /// diff pane のヘッダに出す `jj show --no-patch` 相当。
    pub fn show(&self, rev: &str) -> Result<String> {
        self.show_with_globals(rev, &["--no-pager", "--color", "always"])
    }

    fn show_with_globals(&self, rev: &str, globals: &[&str]) -> Result<String> {
        self.exec(
            globals,
            &["show", "-r", rev, "--no-patch"],
            OutputCapture::DiffPane,
        )
    }

    /// 完全なコミット ID が既知なので、先読みでは作業コピーをスナップショットしない。
    pub(crate) fn warm_metadata(&self, commit_id: &str) -> (Result<Vec<DiffFile>>, Result<String>) {
        (
            self.diff_summary_with_globals(
                commit_id,
                &["--ignore-working-copy", "--no-pager", "--color", "never"],
            ),
            self.show_with_globals(
                commit_id,
                &["--ignore-working-copy", "--no-pager", "--color", "always"],
            ),
        )
    }

    /// working copy の状態 (status bar 用)。
    pub fn status_line(&self) -> Result<String> {
        let out = self.read(&[
            "log",
            "-r",
            "@",
            "--no-graph",
            "--template",
            r#"change_id.shortest(8) ++ if(empty, " (empty)", "") ++ if(conflict, " (conflict)", "")"#,
        ])?;
        Ok(out.trim().to_string())
    }

    pub fn new_child(&self, rev: &str, message: Option<&str>) -> Result<String> {
        let mut args = vec!["new", rev];
        if let Some(message) = message {
            args.push("-m");
            args.push(message);
        }
        self.write(&args)
    }

    pub fn edit(&self, rev: &str) -> Result<String> {
        self.write(&["edit", rev])
    }

    pub fn describe(&self, rev: &str, message: &str) -> Result<String> {
        self.write(&["describe", "-r", rev, "-m", message])
    }

    /// working copy (`@`) を describe して新しい空 change に進む、
    /// `describe` + `new_child` の 1 操作版 (`jj commit`)。
    ///
    /// `-r` は渡さない: `jj commit` は working copy 専用の操作で、他の
    /// change を指す使い方は無い。呼び出し側 (`App::prompt_commit`) が
    /// 選択中 change が `@` であることを事前に確かめる。
    pub fn commit(&self, message: &str) -> Result<String> {
        self.write(&["commit", "-m", message])
    }

    /// `from` の変更を `into` に押し込む。
    ///
    /// `--use-destination-message` を常に付ける: 付けないと「両方に
    /// description があって source が空になる」ケースで jj が editor を
    /// 開こうとする。TUI からは description を後で `describe` すれば
    /// いいので、暗黙 editor より destination の文言を残す方が安全。
    pub fn squash(&self, from: &str, into: &str) -> Result<String> {
        self.write(&[
            "squash",
            "--use-destination-message",
            "--from",
            from,
            "--into",
            into,
        ])
    }

    pub fn rebase(&self, rev: &str, onto: &str, mode: RebaseMode) -> Result<String> {
        self.write(&["rebase", mode.flag(), rev, "--onto", onto])
    }

    pub fn abandon(&self, rev: &str) -> Result<String> {
        self.write(&["abandon", rev])
    }

    /// `path` について、選択中 change (`rev`) に対する変更だけを取り消す。
    /// files pane での「このファイルの変更を破棄する」に対応する。
    pub fn restore_file(&self, rev: &str, paths: &[String]) -> Result<String> {
        let filesets = root_filesets(paths);
        let mut args = vec!["restore", "--changes-in", rev];
        args.extend(filesets.iter().map(String::as_str));
        self.write(&args)
    }

    /// `path` について、`rev` にある変更を祖先の mutable な change へ
    /// 分散させる (`jj absorb`)。files pane での「このファイルの変更を
    /// 適切な commit に配る」に対応する。
    pub fn absorb_file(&self, rev: &str, paths: &[String]) -> Result<String> {
        let filesets = root_filesets(paths);
        let mut args = vec!["absorb", "--from", rev];
        args.extend(filesets.iter().map(String::as_str));
        self.write(&args)
    }

    /// 選択パスを完全一致 fileset エイリアスとして作業コピー外の一時設定に
    /// 書き出し、読み取り専用の `jj file list -r @` で `@` の追跡集合と完全一致
    /// することを検証する。書き込み前に不一致を検出して中止できる。
    pub fn prepare_file_untrack(&self, paths: &[String]) -> Result<UntrackBatch<'_>> {
        let mut ordered = Vec::new();
        let mut expected = BTreeSet::new();
        for raw in paths {
            let norm = normalize_repo_path(raw);
            if expected.insert(norm.clone()) {
                ordered.push(norm);
            }
        }
        if ordered.is_empty() {
            bail!("no paths selected to untrack");
        }
        let expr = fileset_union_expr(&ordered);
        let toml = fileset_alias_toml(UNTRACK_ALIAS, &expr);
        let config = write_untrack_config(self.root(), &std::env::temp_dir(), &toml)?;

        let out = self.exec_with_config(
            READ_GLOBALS,
            Some(config.path()),
            &[
                "--ignore-working-copy",
                "file",
                "list",
                "-r",
                "@",
                "--template",
                FILE_LIST_JSON_TEMPLATE,
                UNTRACK_ALIAS,
            ],
        )?;
        let matched = parse_file_list(&out)?;
        if matched != expected {
            let missing = expected.difference(&matched).count();
            let unexpected = matched.difference(&expected).count();
            bail!(
                "selected paths did not match tracked files in @ ({missing} missing, {unexpected} unexpected)"
            );
        }
        Ok(UntrackBatch {
            jj: self,
            config,
            expected,
        })
    }

    /// エイリアスを使わず、再スナップショットせずに記録済みの `@` と照合する。
    fn reconcile_untrack_progress(&self, expected: &BTreeSet<String>) -> UntrackProgress {
        let Ok(out) = self.read(&[
            "--ignore-working-copy",
            "file",
            "list",
            "-r",
            "@",
            "--template",
            FILE_LIST_JSON_TEMPLATE,
        ]) else {
            return UntrackProgress::Unknown;
        };
        let Ok(tracked) = parse_file_list(&out) else {
            return UntrackProgress::Unknown;
        };
        let remaining = expected.iter().filter(|p| tracked.contains(*p)).count();
        let untracked = expected.len().saturating_sub(remaining);
        UntrackProgress::Verified {
            untracked,
            remaining,
        }
    }

    /// 作業コピーでの追跡を外す (`jj file untrack`)。`i` は先に
    /// `.gitignore` へ追記し、`I` は変更しない。ignore 済みでも auto-track
    /// 対象外でもなく、直ちに再追跡されるパスがあると jj はバッチ全体を拒否する。
    pub fn file_untrack(&self, paths: &[String]) -> Result<String> {
        let batch = self.prepare_file_untrack(paths)?;
        batch.execute().map_err(|err| anyhow::anyhow!("{err}"))
    }

    pub fn bookmark_set(&self, name: &str, rev: &str) -> Result<String> {
        // `bookmark set` は既存 bookmark の移動も新規作成も担う
        // (`create` は既存だとエラー)。TUI から「この change に名前を
        // 置く」操作としては set の方が期待に合う。
        // `--allow-backwards` は付けない: 祖先方向への移動は事故の可能性
        // が高く、jj のガードを残しておく価値がある。
        self.write(&["bookmark", "set", name, "-r", rev])
    }

    pub fn undo(&self) -> Result<String> {
        self.write(&["undo"])
    }

    pub fn redo(&self) -> Result<String> {
        self.write(&["redo"])
    }

    /// 直近の operation 説明 (status bar 用)。
    pub fn last_operation(&self) -> Result<String> {
        let out = self.read(&[
            "op",
            "log",
            "--no-graph",
            "--limit",
            "1",
            "--template",
            r#"description.first_line()"#,
        ])?;
        Ok(out.trim().to_string())
    }

    /// 既存 bookmark 名の一覧 (bookmark 入力の補完候補)。
    pub fn bookmarks(&self) -> Result<Vec<String>> {
        let out = self.read(&["bookmark", "list", "--template", r#"name ++ "\n""#])?;
        let mut names: BTreeSet<String> = BTreeSet::new();
        for line in out.lines() {
            let name = line.trim();
            if !name.is_empty() {
                names.insert(name.to_string());
            }
        }
        Ok(names.into_iter().collect())
    }
}

const READ_GLOBALS: &[&str] = &["--no-pager", "--color", "never"];

const WRITE_GLOBALS: &[&str] = &[
    "--no-pager",
    "--color",
    "never",
    "--config",
    "ui.editor=shikigami-refuses-to-open-an-editor",
];

/// `jj log` の出力を行単位で [`Row`] に分解する。
///
/// 独立関数にしてあるのは、jj を起動せずにパーサだけテストするため。
pub fn parse_log(out: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for line in out.lines() {
        match line.split_once(RECORD) {
            Some((graph, data)) => match parse_change(data) {
                Some(change) => rows.push(Row::Change {
                    graph: graph.to_string(),
                    change,
                }),
                // フィールド数が足りない = template と構造体がずれている。
                // 行を捨てて graph だけ残すと log が無言で欠けるので、
                // 生の行を connector として見せて気付けるようにする。
                None => rows.push(Row::Connector(line.to_string())),
            },
            None => {
                // 末尾の空行は graph でもデータでもないので落とす。
                if !line.trim().is_empty() {
                    rows.push(Row::Connector(line.to_string()));
                }
            }
        }
    }
    rows
}

/// 1 行 1 JSON オブジェクトのファイル情報を読む。
/// 実パスの欠けた古い形式や壊れた出力は、表示から推測せずエラーにする。
pub fn parse_diff_summary(out: &str) -> Result<Vec<DiffFile>> {
    out.lines()
        .map(|line| serde_json::from_str(line).context("cannot parse jj diff file entry"))
        .collect()
}

/// 1 行 1 JSON 文字列の `jj file list --template 'json(path) ++ "\n"'` を読む。
fn parse_file_list(out: &str) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let path: String = serde_json::from_str(line).context("cannot parse jj file list entry")?;
        paths.insert(path);
    }
    Ok(paths)
}

fn parse_change(data: &str) -> Option<Change> {
    let mut f = data.split(FIELD);
    let id = f.next()?.to_string();
    let short_id = f.next()?.to_string();
    let commit_id = f.next()?.to_string();
    let full_commit_id = f.next()?.to_string();
    let is_working_copy = f.next()? == "1";
    let is_empty = f.next()? == "1";
    let has_conflict = f.next()? == "1";
    let is_immutable = f.next()? == "1";
    let author = f.next()?.to_string();
    let timestamp = f.next()?.to_string();
    let bookmarks = f.next()?;
    let description = f.next()?.trim_end().to_string();
    if id.is_empty() {
        return None;
    }
    Some(Change {
        id,
        short_id,
        commit_id,
        full_commit_id,
        is_working_copy,
        is_empty,
        has_conflict,
        is_immutable,
        author,
        timestamp,
        bookmarks: bookmarks
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        description,
    })
}

/// パスを `root:"…"` のリテラル fileset に包む。`jj` のコマンドライン引数は
/// fileset として解釈されるので、素のパスだと `%` / 空白 / 括弧などを含む
/// ファイル名 (例: `token%3Adefault%3Aa@b.com`) で構文エラーになる。
/// `root:` は cwd にも依存しない。
fn root_filesets(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|path| literal_fileset(path, "root"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fields: &[&str]) -> String {
        fields.join(&FIELD.to_string())
    }

    #[test]
    fn parses_graph_prefix_and_fields() {
        let line = format!(
            "@  {RECORD}{}",
            record(&[
                "qxymuzsqabc",
                "qxymuzsq",
                "5e3b8962",
                "5e3b896200000000000000000000000000000000",
                "1",
                "0",
                "0",
                "0",
                "yukimemi",
                "2026-09-19 00:00",
                "main,push-abc",
                "first change",
            ])
        );
        let rows = parse_log(&line);
        let Row::Change { graph, change } = &rows[0] else {
            panic!("expected a change row, got {:?}", rows[0]);
        };
        // graph はマーカーより前の生テキスト。空白も含めて保つ
        // (`│ ○  ` のインデントが崩れると枝が繋がらなく見える)。
        assert_eq!(graph, "@  ");
        assert_eq!(change.id, "qxymuzsqabc");
        assert_eq!(
            change.full_commit_id,
            "5e3b896200000000000000000000000000000000"
        );
        assert!(change.is_working_copy);
        assert!(!change.is_empty);
        assert_eq!(change.bookmarks, vec!["main", "push-abc"]);
        assert_eq!(change.description, "first change");
    }

    #[test]
    fn connector_lines_carry_no_change() {
        // merge の枝行はマーカーを含まない。選択対象から外したいので
        // Change ではなく Connector になる必要がある。
        let rows = parse_log("├─╮\n");
        assert_eq!(rows, vec![Row::Connector("├─╮".to_string())]);
    }

    #[test]
    fn empty_description_and_no_bookmarks_are_not_fabricated() {
        let line = format!(
            "○  {RECORD}{}",
            record(&[
                "zzz",
                "zzz",
                "0000",
                "0000000000000000000000000000000000000000",
                "0",
                "1",
                "0",
                "1",
                "",
                "1970-01-01 00:00",
                "",
                "",
            ])
        );
        let rows = parse_log(&line);
        let Row::Change { change, .. } = &rows[0] else {
            panic!("expected a change row");
        };
        assert!(change.bookmarks.is_empty());
        assert_eq!(change.description, "");
        // 表示だけプレースホルダに替える。description 自体は空のまま
        // (`describe` の初期値に "(no description set)" を流し込まない)。
        assert_eq!(change.summary(), "(no description set)");
        assert!(change.is_immutable);
    }

    #[test]
    fn trailing_blank_lines_are_dropped() {
        assert!(parse_log("\n\n").is_empty());
    }

    #[test]
    fn ai_patch_outlives_diff_pane_output_limit() {
        use std::io::Write;

        let (repo, _) = crate::testutil::repo_or_skip!();
        let dir = repo.path();
        crate::testutil::run(
            dir,
            &[
                "config",
                "set",
                "--repo",
                "snapshot.max-new-file-size",
                "40MiB",
            ],
        );
        let mut file = std::fs::File::create(dir.join("large.txt")).unwrap();
        let mut line = [b'x'; 1024];
        line[1023] = b'\n';
        let block = line.repeat(1024);
        for _ in 0..32 {
            file.write_all(&block).unwrap();
        }
        file.write_all(b"end of large patch\n").unwrap();
        drop(file);
        crate::testutil::run(dir, &["new"]);

        let jj = Jj::discover(dir).unwrap();
        let rows = jj.log(Some("@-")).unwrap();
        let Row::Change { change, .. } = &rows[0] else {
            panic!("expected the change with the large patch");
        };
        let rev = &change.full_commit_id;
        let patch = jj.diff_plain(rev).unwrap();
        assert!(patch.ends_with("+end of large patch\n"));
        drop(patch);

        let err = jj.diff_git_full(rev).unwrap_err();
        assert!(
            format!("{err:#}").contains("diff output exceeds 32 MiB"),
            "{err:#}"
        );
    }

    #[test]
    fn diff_summary_reads_structured_paths_and_rejects_legacy_entries() {
        let files = parse_diff_summary(concat!(
            r#"{"status":"R","path":"{literal => dir}/{a.txt => b.txt}","source_path":"{literal => dir}/a.txt","destination_path":"{literal => dir}/b.txt"}"#, "\n",
            r#"{"status":"R","path":"{literal => dir}/{a.txt => b.txt}","source_path":"literal/{a.txt => b.txt}","destination_path":"dir/{a.txt => b.txt}"}"#, "\n",
        )).unwrap();
        assert_eq!(files[0].path, files[1].path);
        assert_eq!(
            files[0].diff_paths(),
            ["{literal => dir}/a.txt", "{literal => dir}/b.txt"]
        );
        assert_eq!(
            files[1].diff_paths(),
            ["literal/{a.txt => b.txt}", "dir/{a.txt => b.txt}"]
        );
        assert_eq!(files[0].target_path(), "{literal => dir}/b.txt");
        assert_eq!(
            files[1].restore_paths(),
            ["literal/{a.txt => b.txt}", "dir/{a.txt => b.txt}"]
        );
        assert!(parse_diff_summary(r#"{"status":"R","path":"{a => b}"}"#).is_err());
        assert!(parse_diff_summary("R {a => b}\n").is_err());
        assert!(parse_diff_summary("").unwrap().is_empty());
    }

    #[test]
    fn target_path_uses_the_new_side_of_a_rename() {
        assert_eq!(
            DiffFile {
                status: 'M',
                path: "src/app.rs".to_string(),
                source_path: "src/app.rs".to_string(),
                destination_path: "src/app.rs".to_string()
            }
            .target_path(),
            "src/app.rs"
        );
        // jj は共通部分が無くても必ず `{}` で囲む (git の diffstat と同じ
        // 圧縮記法): `jj diff --summary` の実出力に合わせたケース。
        assert_eq!(
            DiffFile {
                status: 'R',
                path: "{from.rs => to.rs}".to_string(),
                source_path: "from.rs".to_string(),
                destination_path: "to.rs".to_string()
            }
            .target_path(),
            "to.rs"
        );
        // 共通の prefix/suffix があるケース (実際に jj が出す形)。
        assert_eq!(
            DiffFile {
                status: 'R',
                path: "sub/{a.rs => b.rs}".to_string(),
                source_path: "sub/a.rs".to_string(),
                destination_path: "sub/b.rs".to_string()
            }
            .target_path(),
            "sub/b.rs"
        );
        assert_eq!(
            DiffFile {
                status: 'R',
                path: "{old => new}/file.rs".to_string(),
                source_path: "old/file.rs".to_string(),
                destination_path: "new/file.rs".to_string()
            }
            .target_path(),
            "new/file.rs"
        );
    }

    #[test]
    fn restore_paths_includes_both_sides_of_a_rename() {
        assert_eq!(
            DiffFile {
                status: 'M',
                path: "src/app.rs".to_string(),
                source_path: "src/app.rs".to_string(),
                destination_path: "src/app.rs".to_string()
            }
            .restore_paths(),
            vec!["src/app.rs"]
        );
        assert_eq!(
            DiffFile {
                status: 'R',
                path: "{from.rs => to.rs}".to_string(),
                source_path: "from.rs".to_string(),
                destination_path: "to.rs".to_string()
            }
            .restore_paths(),
            vec!["from.rs", "to.rs"]
        );
        assert_eq!(
            DiffFile {
                status: 'R',
                path: "sub/{a.rs => b.rs}".to_string(),
                source_path: "sub/a.rs".to_string(),
                destination_path: "sub/b.rs".to_string()
            }
            .restore_paths(),
            vec!["sub/a.rs", "sub/b.rs"]
        );
    }

    #[test]
    fn structured_paths_ignore_compressed_rename_and_copy_display() {
        for (display, old, new) in [
            (
                "home/.gemini/antigravity-cli/{settings.json => settings.base.json}",
                "home/.gemini/antigravity-cli/settings.json",
                "home/.gemini/antigravity-cli/settings.base.json",
            ),
            ("{old => new}/file.rs", "old/file.rs", "new/file.rs"),
            ("src/{old => new}.rs", "src/old.rs", "src/new.rs"),
            ("dir/{sub => }/file.txt", "dir/sub/file.txt", "dir/file.txt"),
            ("dir/{ => sub}/file.txt", "dir/file.txt", "dir/sub/file.txt"),
            ("{sub => }/file.txt", "sub/file.txt", "file.txt"),
            ("{ => sub}/file.txt", "file.txt", "sub/file.txt"),
            ("dir/{sub => }", "dir/sub", "dir"),
            ("dir/{ => sub}", "dir", "dir/sub"),
            ("{old => }file.txt", "oldfile.txt", "file.txt"),
            ("{ => new}file.txt", "file.txt", "newfile.txt"),
            (
                "src/{ => nested/}file.rs",
                "src/file.rs",
                "src/nested/file.rs",
            ),
            (
                "src/{nested/ => }file.rs",
                "src/nested/file.rs",
                "src/file.rs",
            ),
            (
                "{literal}/src/{a => b}.rs",
                "{literal}/src/a.rs",
                "{literal}/src/b.rs",
            ),
            ("{a => b}", "a", "b"),
            (
                "literal => dir/{a.txt => b.txt}",
                "literal => dir/a.txt",
                "literal => dir/b.txt",
            ),
            (
                "one => two/three => four/{a.txt => b.txt}",
                "one => two/three => four/a.txt",
                "one => two/three => four/b.txt",
            ),
            (
                "literal => dir/{old => new}/suffix => dir/file.txt",
                "literal => dir/old/suffix => dir/file.txt",
                "literal => dir/new/suffix => dir/file.txt",
            ),
            ("{old.txt => new}name.txt}", "old.txt", "new}name.txt"),
            ("{old.txt => new/name}.txt}", "old.txt", "new/name}.txt"),
            (
                "src/{old.txt => new}name.txt}/tail/{literal}.txt",
                "src/old.txt/tail/{literal}.txt",
                "src/new}name.txt/tail/{literal}.txt",
            ),
            ("{old.txt => new{brace.txt}", "old.txt", "new{brace.txt"),
            ("{old.txt => new{brace}.txt}", "old.txt", "new{brace}.txt"),
            ("{old{brace.txt => new.txt}", "old{brace.txt", "new.txt"),
            ("{old/{brace.txt => new.txt}", "old/{brace.txt", "new.txt"),
            ("{old}brace.txt => new.txt}", "old}brace.txt", "new.txt"),
            (
                "{literal}/src/{old}brace.txt => new.txt}",
                "{literal}/src/old}brace.txt",
                "{literal}/src/new.txt",
            ),
            ("{old{brace}.txt => new.txt}", "old{brace}.txt", "new.txt"),
            ("{{brace.txt => new.txt}", "{brace.txt", "new.txt"),
            (
                "{literal}/src/{old/{brace.txt => new.txt}",
                "{literal}/src/old/{brace.txt",
                "{literal}/src/new.txt",
            ),
            (
                "nested/{old{brace.txt => new.txt}",
                "nested/old{brace.txt",
                "nested/new.txt",
            ),
            (
                "{literal}/src/{old{brace.txt => new.txt}",
                "{literal}/src/old{brace.txt",
                "{literal}/src/new.txt",
            ),
        ] {
            for status in ['R', 'C'] {
                let file = DiffFile {
                    status,
                    path: display.to_owned(),
                    source_path: old.to_owned(),
                    destination_path: new.to_owned(),
                };
                assert_eq!(file.diff_paths(), [old, new], "{display}");
                assert_eq!(file.target_path(), new, "{display}");
            }
        }
        for status in ['M', 'A', 'D'] {
            let path = "literal/{old => new}.rs";
            let file = DiffFile {
                status,
                path: path.to_owned(),
                source_path: path.to_owned(),
                destination_path: path.to_owned(),
            };
            assert_eq!(file.diff_paths(), [path]);
            assert_eq!(file.target_path(), path);
        }
    }

    #[test]
    #[cfg(windows)]
    fn windows_display_uses_canonical_structured_paths() {
        for (display, old, new) in [
            (r"{sub => }\file.txt", r"sub\file.txt", "file.txt"),
            (
                r"src\{old.txt => new}name.txt}\tail\{literal}.txt",
                r"src\old.txt\tail\{literal}.txt",
                r"src\new}name.txt\tail\{literal}.txt",
            ),
            (r"{old\{brace.txt => new.txt}", r"old\{brace.txt", "new.txt"),
            (
                r"{literal}\src\{old\{brace.txt => new.txt}",
                r"{literal}\src\old\{brace.txt",
                r"{literal}\src\new.txt",
            ),
            (r"{ => sub}\file.txt", "file.txt", r"sub\file.txt"),
            (
                r"dir\{sub => }\file.txt",
                r"dir\sub\file.txt",
                r"dir\file.txt",
            ),
            (
                r"dir\{ => sub}\file.txt",
                r"dir\file.txt",
                r"dir\sub\file.txt",
            ),
            (r"dir\{sub => }", r"dir\sub", "dir"),
            (r"dir\{ => sub}", "dir", r"dir\sub"),
            (
                r"nested\{old{brace.txt => new.txt}",
                r"nested\old{brace.txt",
                r"nested\new.txt",
            ),
        ] {
            let file = DiffFile {
                status: 'R',
                path: display.to_owned(),
                source_path: old.replace('\\', "/"),
                destination_path: new.replace('\\', "/"),
            };
            assert_eq!(
                file.diff_paths(),
                [old.replace('\\', "/"), new.replace('\\', "/")],
                "{display}"
            );
            assert_eq!(file.target_path(), new.replace('\\', "/"), "{display}");
            assert_eq!(
                literal_fileset(&file.target_path(), "root-file"),
                format!("root-file:\"{}\"", new.replace('\\', "/"))
            );
        }
    }

    #[test]
    fn structured_paths_preserve_escaped_filenames() {
        let files = parse_diff_summary(
            r#"{"status":"R","path":"display only","source_path":"quote\"and\\slash\t\n.txt ","destination_path":"new\\name.txt "}"#,
        ).unwrap();
        assert_eq!(
            files[0].diff_paths(),
            ["quote\"and\\slash\t\n.txt ", "new\\name.txt "]
        );
        assert_eq!(files[0].target_path(), "new\\name.txt ");
    }

    #[test]
    fn literal_filesets_quote_operators_and_escape_strings() {
        for kind in ["root-file", "root"] {
            for path in ["a b", "(a|b)&~c", "glob:*.rs", "{a => b}", "-option"] {
                assert_eq!(literal_fileset(path, kind), format!("{kind}:\"{path}\""));
            }
            assert_eq!(literal_fileset("a\"b", kind), format!(r#"{kind}:"a\"b""#));
            assert_eq!(
                literal_fileset("a\tb\nc\r", kind),
                format!(r#"{kind}:"a\tb\nc\r""#)
            );
            if cfg!(windows) {
                assert_eq!(
                    literal_fileset(r"dir\file", kind),
                    format!(r#"{kind}:"dir/file""#)
                );
            } else {
                assert_eq!(
                    literal_fileset(r"dir\file", kind),
                    format!(r#"{kind}:"dir\\file""#)
                );
            }
        }
    }

    #[test]
    fn summarize_command_omits_paths_and_flags() {
        assert_eq!(
            summarize_command(&["file", "untrack", "root:\"a\"", "root:\"b\""]),
            "file untrack"
        );
        assert_eq!(
            summarize_command(&["file", "list", "-r", "@", "--template", "json(path)"]),
            "file list"
        );
        assert_eq!(
            summarize_command(&["restore", "--changes-in", "@", "root:\"a\""]),
            "restore"
        );
        assert_eq!(
            summarize_command(&["bookmark", "move", "main", "--to", "@-"]),
            "bookmark move"
        );
        assert_eq!(summarize_command(&["--quiet", "git", "fetch"]), "git fetch");
        assert_eq!(summarize_command(&[]), "");
    }

    #[test]
    fn toml_alias_serialization_escapes_special_characters_and_unicode() {
        let paths = vec![
            "a b/(c)&~d.txt".to_string(),
            "日本語/file\"quote.txt".to_string(),
            "tab\there.txt".to_string(),
        ];
        let expr = fileset_union_expr(&paths);
        assert_eq!(
            expr,
            r#"root-file:"a b/(c)&~d.txt" | root-file:"日本語/file\"quote.txt" | root-file:"tab\there.txt""#
        );
        let toml = fileset_alias_toml(UNTRACK_ALIAS, &expr);
        assert!(toml.starts_with("[fileset-aliases]\n__shikigami_untrack_targets = \""));
        assert!(toml.contains(r#"\\\"quote.txt"#));
        assert!(toml.contains(r#"\\there.txt"#));
        assert!(toml.contains("日本語"));
    }

    #[test]
    fn parse_file_list_reads_json_lines_and_rejects_malformed_input() {
        let parsed = parse_file_list(concat!(
            "\"a b.txt\"\n",
            "\"nested/日本語 [1].txt\"\n",
            "\n",
            "\"quote\\\"slash\\\\.txt\"\n",
        ))
        .unwrap();
        let expected: BTreeSet<String> = ["a b.txt", "nested/日本語 [1].txt", "quote\"slash\\.txt"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(parsed, expected);
        assert!(parse_file_list("not-json\n").is_err());
    }

    #[test]
    fn untrack_temp_config_rejects_working_copy_directory_and_cleans_up() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("repo");
        let inside = root.join("temp");
        std::fs::create_dir_all(&inside).unwrap();
        let root = root.canonicalize().unwrap();
        let contents = "[fileset-aliases]\nfoo = \"none()\"\n";
        let err = write_untrack_config(&root, &inside, contents).unwrap_err();
        assert!(err.to_string().contains("outside the working copy"));
        assert_eq!(std::fs::read_dir(inside).unwrap().count(), 0);

        let config = write_untrack_config(&root, base.path(), contents).unwrap();
        let path = config.path().to_path_buf();
        assert!(!path.starts_with(root));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        drop(config);
        assert!(!path.exists(), "temp config should be removed on drop");
    }

    #[test]
    fn untrack_checks_recorded_paths_without_snapshotting() {
        let (tmp, _) = crate::testutil::repo_or_skip!();
        let dir = tmp.path();
        let path = dir.join("target.txt");
        std::fs::write(&path, "tracked").unwrap();
        crate::testutil::run(dir, &["file", "track", "target.txt"]);
        let jj = Jj::discover(dir).unwrap();
        let target = "target.txt".to_string();

        // 作業ツリーから消えても、次のスナップショットまでは @ に記録されている。
        std::fs::remove_file(path).unwrap();
        let batch = jj
            .prepare_file_untrack(std::slice::from_ref(&target))
            .unwrap();
        assert_eq!(
            jj.reconcile_untrack_progress(&BTreeSet::from([target])),
            UntrackProgress::Verified {
                untracked: 0,
                remaining: 1,
            }
        );
        drop(batch);
    }

    #[test]
    fn rebase_mode_cycles_and_maps_to_jj_flags() {
        assert_eq!(RebaseMode::Revision.flag(), "-r");
        assert_eq!(RebaseMode::Revision.next(), RebaseMode::Source);
        assert_eq!(RebaseMode::Source.next(), RebaseMode::Branch);
        assert_eq!(RebaseMode::Branch.next(), RebaseMode::Revision);
    }

    #[test]
    fn discover_walks_up_to_the_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".jj")).unwrap();

        let jj = Jj::discover(&nested).unwrap();
        assert_eq!(
            jj.root().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn discover_ignores_git_only_repos() {
        // git だけの repo で開けてしまうと、TUI は起動するのに全操作が
        // 失敗する。ここで落として「jj repo ではない」と言う方が親切。
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let err = Jj::discover(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("no jj repo found"),
            "unexpected error: {err}"
        );
    }
}
