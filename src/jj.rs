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

/// `jj diff --summary` の 1 行。files pane (lazygit のファイル一覧相当) の
/// 1 項目に対応する。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffFile {
    /// jj のステータス文字 (`M`/`A`/`D`/`R`/`C`)。
    pub status: char,
    /// リネームは `old => new` の形のままここに入る。
    pub path: String,
}

impl DiffFile {
    /// jj のファイルセット引数として渡せるパス。
    ///
    /// リネーム/コピーは `path` に `"old => new"` がそのまま入っている
    /// ので、そのまま jj に渡すとファイルセットとして解釈できない。
    /// 現在その change に存在するのは新パス側なので、`=>` の右側を返す。
    pub fn target_path(&self) -> &str {
        match self.path.split_once(" => ") {
            Some((_old, new)) => new,
            None => &self.path,
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
        self.exec(&["--no-pager", "--color", "never"], args)
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
        self.exec(
            &[
                "--no-pager",
                "--color",
                "never",
                "--config",
                "ui.editor=shikigami-refuses-to-open-an-editor",
            ],
            args,
        )
    }

    fn exec(&self, globals: &[&str], args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("jj");
        cmd.current_dir(&self.root);
        cmd.args(globals);
        cmd.args(args);
        let out = cmd
            .output()
            .with_context(|| format!("failed to run `jj {}`", args.join(" ")))?;
        if !out.status.success() {
            // jj のエラーは stderr に人間向けの Error/Hint で出る。
            // それをそのまま status bar に載せたいので、整形せず返す。
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.trim();
            if msg.is_empty() {
                bail!("`jj {}` failed with {}", args.join(" "), out.status);
            }
            bail!("{msg}");
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
    /// `path` を渡すと、その 1 ファイル分だけに絞った diff になる
    /// (jj のファイルセット引数をそのまま流す)。
    pub fn diff(&self, rev: &str, path: Option<&str>) -> Result<String> {
        let mut args = vec!["diff", "-r", rev];
        if let Some(path) = path {
            args.push(path);
        }
        self.exec(&["--no-pager", "--color", "always"], &args)
    }

    /// [`Jj::diff`] の色なし版。AI コマンドなど、ANSI を混ぜたくない
    /// 消費者へ渡す用途に使う。常に change 全体 (path 絞り込みなし)。
    pub fn diff_plain(&self, rev: &str) -> Result<String> {
        self.read(&["diff", "-r", rev, "--git"])
    }

    /// 選択中 change で変更されたファイルの一覧 (`jj diff --summary`)。
    /// lazygit のようにファイル単位で diff を選べるようにするための入口。
    pub fn diff_summary(&self, rev: &str) -> Result<Vec<DiffFile>> {
        let out = self.read(&["diff", "-r", rev, "--summary"])?;
        Ok(parse_diff_summary(&out))
    }

    /// diff pane のヘッダに出す `jj show --no-patch` 相当。
    pub fn show(&self, rev: &str) -> Result<String> {
        self.exec(
            &["--no-pager", "--color", "always"],
            &["show", "-r", rev, "--no-patch"],
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
    pub fn restore_file(&self, rev: &str, path: &str) -> Result<String> {
        self.write(&["restore", "--changes-in", rev, path])
    }

    /// `path` について、`rev` にある変更を祖先の mutable な change へ
    /// 分散させる (`jj absorb`)。files pane での「このファイルの変更を
    /// 適切な commit に配る」に対応する。
    pub fn absorb_file(&self, rev: &str, path: &str) -> Result<String> {
        self.write(&["absorb", "--from", rev, path])
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

/// `jj diff --summary` の出力を [`DiffFile`] へ分解する。
///
/// 各行は `<status><space><path>` (例: `M src/app.rs`, `R old => new`)。
/// `parse_log` と同じ理由で、jj を起動せずにパーサだけテストできるよう
/// 独立関数にしてある。
pub fn parse_diff_summary(out: &str) -> Vec<DiffFile> {
    out.lines()
        .filter_map(|line| {
            let mut chars = line.chars();
            let status = chars.next()?;
            let path = chars.as_str().trim();
            if path.is_empty() {
                return None;
            }
            Some(DiffFile {
                status,
                path: path.to_string(),
            })
        })
        .collect()
}

fn parse_change(data: &str) -> Option<Change> {
    let mut f = data.split(FIELD);
    let id = f.next()?.to_string();
    let short_id = f.next()?.to_string();
    let commit_id = f.next()?.to_string();
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
    fn diff_summary_parses_status_and_path_including_renames() {
        let files = parse_diff_summary("M src/app.rs\nA new.rs\nD old.rs\nR from.rs => to.rs\n");
        assert_eq!(
            files,
            vec![
                DiffFile {
                    status: 'M',
                    path: "src/app.rs".to_string()
                },
                DiffFile {
                    status: 'A',
                    path: "new.rs".to_string()
                },
                DiffFile {
                    status: 'D',
                    path: "old.rs".to_string()
                },
                DiffFile {
                    status: 'R',
                    path: "from.rs => to.rs".to_string()
                },
            ]
        );
    }

    #[test]
    fn target_path_uses_the_new_side_of_a_rename() {
        assert_eq!(
            DiffFile {
                status: 'M',
                path: "src/app.rs".to_string(),
            }
            .target_path(),
            "src/app.rs"
        );
        assert_eq!(
            DiffFile {
                status: 'R',
                path: "from.rs => to.rs".to_string(),
            }
            .target_path(),
            "to.rs"
        );
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
