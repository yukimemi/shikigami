//! AI へのコミットメッセージ生成を委譲する薄い境界。
//!
//! shikigami はどの AI ベンダとも直接喋らない。[`AI_CMD_ENV`] に設定した
//! シェルコマンドへ diff を stdin で渡し、その stdout をメッセージとして
//! 使うだけ。理由は `jj.rs` が `jj-lib` ではなく `jj` CLI を叩く理由と同じ:
//! 特定 SDK / API に縛られず、opencode/claude/gemini/aichat/sgpt など
//! 好きな CLI を各自の `~/.config/jj/config.toml` 感覚で繋げられる。
//! 非同期ランタイムや HTTP クライアントを持ち込む必要もない
//! (`Cargo.toml` 冒頭のコメント参照)。

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// AI コマンドを指定する環境変数名。[`crate::app::App::new`] が起動時に
/// 一度だけ読んで `App::ai_cmd` に持つ (毎回 `env::var` を呼ばないので、
/// 実行中の env 書き換えやテストでの差し替えに影響されない)。
pub const AI_CMD_ENV: &str = "SHIKIGAMI_AI_CMD";

/// `cmd` をユーザのシェル経由で起動し、`diff` を stdin で渡してメッセージを
/// 得る。
///
/// シェル経由にするのは、パイプやクォートを含むコマンドライン
/// (`opencode run -m sonnet -` のような引数付きや `... | sed ...`) を
/// そのまま `SHIKIGAMI_AI_CMD` に書けるようにするため。
///
/// stdin への書き込みは別スレッドに逃がす: diff が大きいとき、子プロセスが
/// stdout をあまり出さないまま stdin を読み切る前に親が書き込みでブロック
/// し、同時に子の stdout パイプも埋まって双方待ちになりうる。
pub fn generate_message(cmd: &str, diff: &str) -> Result<String> {
    if cmd.trim().is_empty() {
        bail!("{AI_CMD_ENV} is empty");
    }

    let mut child = shell_command(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {AI_CMD_ENV}"))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let payload = diff.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(payload.as_bytes());
    });

    let out = child
        .wait_with_output()
        .with_context(|| format!("failed to read output of {AI_CMD_ENV}"))?;
    // out は既に取れているので、書き込みスレッド側の結果はここでは
    // 無視してよい (失敗するとしても write_all のエラーは子の exit code /
    // stderr の方に出るはず)。
    let _ = writer.join();

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.trim();
        if msg.is_empty() {
            bail!("{AI_CMD_ENV} failed with {}", out.status);
        }
        bail!("{msg}");
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    // shikigami の入力欄は 1 行なので、複数行の出力は 1 行に畳む。
    let message = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if message.is_empty() {
        bail!("{AI_CMD_ENV} produced no output");
    }
    Ok(message)
}

#[cfg(unix)]
fn shell_command(cmd: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd);
    command
}

#[cfg(windows)]
fn shell_command(cmd: &str) -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C").arg(cmd);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_the_configured_command_and_trims_output() {
        #[cfg(unix)]
        let cmd = "echo 'feat: add thing'";
        #[cfg(windows)]
        let cmd = "echo feat: add thing";
        let out = generate_message(cmd, "diff --git a/x b/x").unwrap();
        assert_eq!(out, "feat: add thing");
    }

    #[test]
    fn empty_command_is_rejected() {
        let err = generate_message("   ", "diff").unwrap_err();
        assert!(err.to_string().contains(AI_CMD_ENV), "{err}");
    }

    #[test]
    fn nonzero_exit_surfaces_stderr() {
        #[cfg(unix)]
        let cmd = "echo boom 1>&2; exit 1";
        #[cfg(windows)]
        let cmd = "echo boom 1>&2 & exit 1";
        let err = generate_message(cmd, "diff").unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn multiline_output_is_folded_into_one_line() {
        #[cfg(unix)]
        let cmd = "printf 'line one\\nline two\\n'";
        #[cfg(windows)]
        let cmd = "echo line one& echo line two";
        let out = generate_message(cmd, "diff").unwrap();
        assert_eq!(out, "line one line two");
    }

    #[test]
    fn stdin_carries_the_diff_through() {
        #[cfg(unix)]
        let cmd = "cat";
        #[cfg(windows)]
        let cmd = "more";
        let out = generate_message(cmd, "hello from diff").unwrap();
        assert_eq!(out, "hello from diff");
    }
}
