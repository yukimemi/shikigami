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
    let _ = terminal.show_cursor();
    result
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    while !app.should_quit {
        // 描画の直前に 1 回だけ diff を取る。j/k の連打中に 1 行ごと
        // `jj diff` を起動しない (app::App::sync_diff 参照)。
        app.sync_diff();
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(POLL)? {
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
