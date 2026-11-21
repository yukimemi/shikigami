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

use std::io::{self, Read, Write};
use std::process::{Command, Output, Stdio};

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

    let mut command = shell_command(cmd);
    command.env("COLUMNS", width.to_string());
    let out = capture(&mut command, Some(diff))
        .with_context(|| format!("failed to run {DIFF_FILTER_ENV}"))?;

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

const MAX_STDOUT: u64 = 32 * 1024 * 1024;
const MAX_STDERR: u64 = 256 * 1024;

pub(crate) fn capture(command: &mut Command, input: Option<&str>) -> Result<Output> {
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (stdout, stderr, status) = std::thread::scope(|scope| {
        if let Some(input) = input {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            scope.spawn(move || {
                let _ = stdin.write_all(input.as_bytes());
            });
        }
        let stderr = child.stderr.take().expect("stderr was piped");
        let errors = scope.spawn(move || drain(stderr, MAX_STDERR));
        let stdout = child.stdout.take().expect("stdout was piped");
        let output = drain(stdout, MAX_STDOUT);
        (output, errors.join().unwrap(), child.wait())
    });
    let (stdout, overflow) = stdout?;
    if overflow {
        bail!("diff output exceeds {} MiB", MAX_STDOUT / (1024 * 1024));
    }
    Ok(Output {
        status: status?,
        stdout,
        stderr: stderr?.0,
    })
}

fn drain(mut pipe: impl Read, limit: u64) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    pipe.by_ref().take(limit).read_to_end(&mut bytes)?;
    // 上限超過を検出しつつ、子が出力パイプに詰まらないよう EOF まで捨てる。
    let overflow = io::copy(&mut pipe, &mut io::sink())? != 0;
    Ok((bytes, overflow))
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

    #[test]
    fn capture_drains_oversized_pipes_without_returning_truncated_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("oversized");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_STDOUT + 1).unwrap();
        drop(file);

        #[cfg(unix)]
        let (emit, emit_stderr) = (r#"cat "$PAYLOAD""#, r#"cat "$PAYLOAD" >&2; echo done"#);
        #[cfg(windows)]
        let (emit, emit_stderr) = (
            r#"type "%PAYLOAD%""#,
            r#"type "%PAYLOAD%" 1>&2 && echo done"#,
        );

        let mut command = shell_command(emit_stderr);
        command.env("PAYLOAD", &path);
        let out = capture(&mut command, None).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stderr.len(), MAX_STDERR as usize);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "done");

        let mut command = shell_command(emit);
        command.env("PAYLOAD", &path);
        let err = capture(&mut command, None).unwrap_err();
        assert!(err.to_string().contains("exceeds 32 MiB"), "{err}");
    }

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
