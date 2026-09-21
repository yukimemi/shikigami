//! files pane の選択ファイルを `$EDITOR` で開くための、環境変数解決と
//! argv 分割だけを切り出したモジュール。
//!
//! `ai.rs`/`diff_filter.rs` はどちらもユーザコマンドをシェル経由で叩く
//! (非対話・パイプ越しの入出力で完結するので、shikigami 自身の端末は
//! 一切明け渡さない)。こちらは事情が違う: vim/nvim/emacs -nw のような
//! エディタは raw mode やカーソル位置を直接操作するので、shikigami が
//! alternate screen に居座ったまま子プロセスを起動すると両者の描画が
//! 衝突して画面が壊れる。実際に alternate screen を抜けて端末を明け渡し、
//! 戻ってきたら復元する処理は `Terminal` を握っている `tui::event_loop`
//! 側でしかできない — ここでは「何を」起動するかの解決だけを持ち、単体で
//! テストできるようにしてある。
//!
//! シェルを経由しない (`sh -c`/`cmd /C` を挟まない) のは意図的: `$EDITOR`
//! に引用符付きの単語を書く運用は稀で、単純な空白区切り
//! (`code --wait` のような追加フラグ) だけ想定すれば足りる。シェル越しに
//! すると `ai.rs` の Windows 版 `shell_command` で踏んだ
//! `cmd /C` の再クォート地雷を、対話プロセス相手にもう一度踏むことになる。

use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

/// エディタを指定する環境変数名。`EDITOR` を優先し、無ければ `VISUAL` を
/// 見る (どちらも Unix の伝統的な慣習: `VISUAL` はフルスクリーンエディタ
/// 用に区別されていた名残りだが、今日日どちらか一方だけ設定している
/// ユーザも多いので両方拾う)。
pub const EDITOR_ENV: &str = "EDITOR";
pub const VISUAL_ENV: &str = "VISUAL";

/// `EDITOR`→`VISUAL` の順で、空でない値を返す。[`crate::app::App::new`] が
/// 起動時に一度だけ呼んで `App::editor_cmd` に持つ (実行中の env 書き換え
/// やテストでの差し替えに影響されないようにするため — `ai_cmd`/
/// `diff_filter_cmd` と同じ方針)。
pub fn resolve() -> Option<String> {
    for key in [EDITOR_ENV, VISUAL_ENV] {
        if let Ok(value) = std::env::var(key)
            && !value.trim().is_empty()
        {
            return Some(value);
        }
    }
    None
}

/// `cmd` を空白区切りで argv に割る。クォート/エスケープは解釈しない —
/// 上のモジュールコメント参照。
fn split(cmd: &str) -> Vec<&str> {
    cmd.split_whitespace().collect()
}

/// `cmd` (`$EDITOR` の値) に `path` を最後の引数として追加した
/// [`Command`] を組み立てる。
pub fn command(cmd: &str, path: &Path) -> Result<Command> {
    let parts = split(cmd);
    let Some((program, args)) = parts.split_first() else {
        bail!("editor command is empty");
    };
    let mut command = Command::new(program);
    command.args(args).arg(path);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_breaks_on_whitespace_only() {
        assert_eq!(split("code --wait"), vec!["code", "--wait"]);
        assert_eq!(split("  vim  "), vec!["vim"]);
        assert_eq!(split(""), Vec::<&str>::new());
    }

    #[test]
    fn command_appends_the_path_as_the_last_argument() {
        let cmd = command("code --wait", Path::new("/tmp/a b.txt")).unwrap();
        // std::process::Command には引数を直接覗く public API が無いので、
        // Debug 表示で argv を確認する (`Command { std: "code" "--wait"
        // "/tmp/a b.txt", … }` のような形式)。
        let debug = format!("{cmd:?}");
        assert!(debug.contains("\"code\""));
        assert!(debug.contains("\"--wait\""));
        assert!(debug.contains("a b.txt"));
    }

    #[test]
    fn command_rejects_an_empty_editor_string() {
        assert!(command("   ", Path::new("/tmp/x")).is_err());
    }
}
