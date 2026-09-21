//! 生の `jj diff` 出力 (ANSI 付き) を外部コマンドで着色し直す薄いフック。
//!
//! `ai.rs` と同じ理由: shikigami は delta / diff-so-fancy のどれとも直接
//! 統合しない。[`DIFF_FILTER_ENV`] に設定したシェルコマンドへ diff を
//! stdin で渡し、その stdout (ANSI 前提) をそのまま diff pane に流し込む
//! だけ。`jj diff --color always` は unified diff 形式なので、delta の
//! ような「unified diff を受け取って着色し直す」ページャ系ツールをその
//! まま挟める。
//!
//! difftastic のように木を直接比較する diff-generator 系はここでは対象
//! 外 — そちらは `ui.diff-formatter` (`Jj::diff` が `--git` を強制しない
//! ことで既に従える) 側の役割。

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// 差分フィルタコマンドを指定する環境変数名。`App::new` が起動時に一度
/// だけ読んで `App::diff_filter_cmd` に持つ。
pub const DIFF_FILTER_ENV: &str = "SHIKIGAMI_DIFF_FILTER";

/// `cmd` をユーザのシェル経由で起動し、`diff` (ANSI 付き) を stdin で
/// 渡してフィルタ後の ANSI テキストを得る。
///
/// `width` は子プロセスの `COLUMNS` 環境変数として渡す。delta の
/// `--side-by-side` はターミナル幅を tty から自動検出するが、パイプ越し
/// では tty が無いため、`delta --width "$COLUMNS"` のようなラッパー越し
/// に diff pane の実幅を渡せるようにしてある。
pub fn apply(cmd: &str, diff: &str, width: u16) -> Result<String> {
    if cmd.trim().is_empty() {
        bail!("{DIFF_FILTER_ENV} is empty");
    }

    let mut child = shell_command(cmd)
        .env("COLUMNS", width.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {DIFF_FILTER_ENV}"))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let payload = diff.to_string();
    // 書き込みは別スレッドに逃がす: diff が大きいとき、子プロセスが
    // stdout をあまり出さないまま stdin を読み切る前に親が書き込みで
    // ブロックし、同時に子の stdout パイプも埋まって双方待ちになりうる
    // (`ai.rs::generate_message` と同じ理由)。
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(payload.as_bytes());
    });

    let out = child
        .wait_with_output()
        .with_context(|| format!("failed to read output of {DIFF_FILTER_ENV}"))?;
    let _ = writer.join();

    if !out.status.success() {
        let stderr = crate::decode::decode(&out.stderr);
        let msg = stderr.trim();
        if msg.is_empty() {
            bail!("{DIFF_FILTER_ENV} failed with {}", out.status);
        }
        bail!("{msg}");
    }

    Ok(crate::decode::decode(&out.stdout))
}

#[cfg(unix)]
fn shell_command(cmd: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd);
    command
}

#[cfg(windows)]
fn shell_command(cmd: &str) -> Command {
    // `ai.rs::shell_command` と同じ理由・同じ対処 (詳細はそちらのコメント
    // 参照): `raw_arg` に加えて文字列全体をもう 1 組の `"..."` で包み、
    // 先頭が `"C:\Program Files\tool.exe" ...` のような引用符付き絶対
    // パスでも `cmd /C` が誤認識しないようにする。
    use std::os::windows::process::CommandExt;
    let mut command = Command::new("cmd");
    command.arg("/C");
    command.raw_arg(format!("\"{cmd}\""));
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn cat_cmd() -> &'static str {
        "cat"
    }
    #[cfg(windows)]
    fn cat_cmd() -> &'static str {
        "more"
    }

    #[test]
    fn passes_diff_through_and_sets_columns() {
        #[cfg(unix)]
        let cmd = "echo \"cols=$COLUMNS\"; cat";
        #[cfg(windows)]
        let cmd = "echo cols=%COLUMNS% && more";
        let out = apply(cmd, "hello\n", 123).unwrap();
        assert!(out.contains("cols=123"), "{out}");
        assert!(out.contains("hello"), "{out}");
    }

    #[test]
    fn empty_cmd_errors() {
        assert!(apply("", "diff", 80).is_err());
    }

    #[test]
    fn failing_cmd_surfaces_stderr() {
        #[cfg(unix)]
        let cmd = "echo boom >&2; exit 1";
        #[cfg(windows)]
        let cmd = "echo boom 1>&2 && exit 1";
        let err = apply(cmd, "diff", 80).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn nonzero_exit_without_stderr_reports_status() {
        let cmd = "exit 3";
        let err = apply(cmd, "diff", 80).unwrap_err();
        assert!(err.to_string().contains("3") || err.to_string().to_lowercase().contains("failed"));
    }

    #[test]
    fn cat_is_available_for_sanity() {
        // 存在確認だけ: プラットフォーム別コマンド名を使っているテストが
        // 環境依存で無意味に落ちていないことのメモ用。
        let _ = cat_cmd();
    }

    // `ai.rs::quoted_argument_with_spaces_survives_cmd_reparsing` と同じ
    // 回帰。`SHIKIGAMI_DIFF_FILTER` に `delta --width "%COLUMNS%"
    // --side-by-side` のような引用符付き引数付きコマンドを書いても、
    // `cmd /C` 越しに引数がバラけないことを確認する。
    #[cfg(windows)]
    #[test]
    fn quoted_argument_with_spaces_survives_cmd_reparsing() {
        let cmd = r#"findstr /C:"needle with spaces""#;
        let out = apply(cmd, "before\nneedle with spaces\nafter\n", 80).unwrap();
        assert!(out.contains("needle with spaces"), "{out}");
    }

    // `ai.rs::command_not_found_error_is_readable_regardless_of_console_locale`
    // と同じ回帰: `SHIKIGAMI_DIFF_FILTER` の実行ファイルが PATH に無いと
    // cmd.exe 自身の (ロケール依存で UTF-8 ではない) 診断メッセージが
    // 出る。CI のロケールに関わらず検証できるよう、文言ではなく置換文字
    // が出ないことを確認する。
    #[cfg(windows)]
    #[test]
    fn command_not_found_error_is_readable_regardless_of_console_locale() {
        let err = apply("nonexistent-tool-shikigami-repro", "diff", 80).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.is_empty(), "{msg}");
        assert!(!msg.contains('\u{FFFD}'), "{msg}");
    }
}
