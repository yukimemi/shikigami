//! 端末の出入りとイベントループ。
//!
//! alternate screen と raw mode の後始末は [`TerminalGuard`] の `Drop` に
//! 寄せてある。`?` で早期 return しても panic しても必ず戻るので、
//! 「TUI が落ちて以降の shell が壊れる」事故が起きない。

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;
use crate::ui;

/// 入力待ちの上限。
///
/// アニメーションは無いので本来ブロックしていて構わないが、上限を
/// 入れておくと SIGWINCH 以外で描画が取り残された場合にも自然に
/// 復帰する。長めにして idle 時の CPU は使わない。
const POLL: Duration = Duration::from_millis(500);

/// 起動時のバックグラウンド `jj log` が終わっていない間だけ使う短い
/// poll 間隔。`POLL` のままだと結果が届いてから最大 500ms 表示が
/// 遅れる — 起動直後だけ詰めて、そこが終わればすぐ `POLL` に戻す。
const STARTUP_POLL: Duration = Duration::from_millis(30);

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = disable_raw_mode();
        let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

pub fn run(app: &mut App) -> Result<()> {
    enable_raw_mode().context("failed to enter raw mode")?;
    // raw mode の直後に guard を作る: 下の execute! や Terminal::new が
    // 失敗しても raw mode のまま抜けない。
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))
        .context("failed to initialise the terminal")?;
    terminal.hide_cursor()?;

    let result = event_loop(&mut terminal, app);
    // 次回起動を速くするための保存。ベストエフォート: これが失敗しても
    // 表示していたセッション自体は正常に終わっているので、終了処理は
    // 続ける。
    app.save_disk_cache();
    let _ = terminal.show_cursor();
    // 起動時 `jj log`/`jj status` の失敗はここで拾う。alternate screen を
    // 抜けたあとにエラーで終了するので、以前の同期呼び出しと同じく
    // 「はっきりしたエラーメッセージを残してプロセスが終了する」形になる。
    result.and_then(|()| match app.take_startup_error() {
        Some(err) => Err(err),
        None => Ok(()),
    })
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    while !app.should_quit {
        // 起動時の `jj log`/`jj status` はバックグラウンドスレッドで
        // 走らせてある (`App::begin_reload`)。終わっていればここで
        // ノンブロッキングに反映する — メインスレッドは一度も
        // ブロックしない。
        app.poll_startup();
        // 描画の直前に 1 回だけ diff を取る。j/k の連打中に 1 行ごと
        // `jj diff` を起動しない (app::App::sync_diff 参照)。SIGWINCH
        // 直後で size 取得自体が失敗することがあるので、その場合は前回
        // フレームの終端に近い幅として 80 桁にフォールバックする。
        let diff_width = terminal
            .size()
            .map(|size| ui::diff_pane_width(size.width))
            .unwrap_or(80);
        app.sync_diff(diff_width);
        terminal.draw(|frame| ui::draw(frame, app))?;
        // `should_quit` (poll_startup が起動失敗で立てることがある) が
        // 既に立っていたら、次ループの先頭で即座に抜ける。ここで
        // `event::poll` を待つと最大 `POLL`/`STARTUP_POLL` 分、来ない
        // キー入力を無駄に待ってから終了することになる。
        let poll_timeout = if app.is_loading() { STARTUP_POLL } else { POLL };
        if !app.should_quit && event::poll(poll_timeout)? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key),
                // Resize は draw が次のループで拾うので、ここでは
                // ループを回すだけでよい。
                Event::Resize(..) => {}
                _ => {}
            }
        }
    }
    Ok(())
}
