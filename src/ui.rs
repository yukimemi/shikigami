//! 描画だけ。状態は持たず、[`App`] を読んで frame に落とす。

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus, Mode, StatusKind};
use crate::jj::{DiffFile, Row};

/// help overlay に出すキー一覧。`doc` と二重管理にならないよう、
/// README / vimdoc はここを参照する形にしてある。
pub const KEYS: &[(&str, &str)] = &[
    ("j / k, ↓ / ↑", "move (log) or scroll (diff)"),
    ("g / G", "first / last"),
    ("Ctrl-d / Ctrl-u", "half-page move"),
    ("Tab", "cycle focus: log -> files -> diff"),
    ("Enter", "jj edit — make the change the working copy"),
    ("n", "jj new — child of the selected change"),
    ("e", "jj describe — edit the description"),
    ("Ctrl-g (in n/e prompt)", "AI: fill message from the diff"),
    ("b", "jj bookmark set"),
    ("m", "mark the selected change as squash/rebase target"),
    ("s", "jj squash — selected into the marked change"),
    (
        "x",
        "jj rebase — selected onto the marked change (or a revset)",
    ),
    ("R", "cycle rebase mode: -r / -s / -b"),
    ("a", "jj abandon"),
    ("u / U", "jj undo / jj redo"),
    ("/", "set the log revset"),
    ("A", "toggle revset all()"),
    ("Ctrl-r", "reload"),
    ("? ", "toggle this help"),
    ("q, Esc", "quit"),
];

pub fn draw(frame: &mut Frame, app: &App) {
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [log_area, right_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
    // files pane はファイル数 + 枠 2 行で伸縮させ、余白を無駄にしない。
    // ただし diff pane を 3 行未満に潰さない上限は付ける。
    let files_max = right_area.height.saturating_sub(3).max(3);
    let files_height = (app.files.len() as u16 + 2).clamp(3, files_max);
    let [files_area, diff_area] =
        Layout::vertical([Constraint::Length(files_height), Constraint::Min(1)]).areas(right_area);

    draw_log(frame, log_area, app);
    draw_files(frame, files_area, app);
    draw_diff(frame, diff_area, app);
    draw_status(frame, status, app);

    match &app.mode {
        Mode::Help => draw_help(frame),
        Mode::Input(input) => draw_input(
            frame,
            &input.prompt,
            &input.value,
            input.action.ai_rev().is_some(),
        ),
        Mode::Confirm(confirm) => draw_confirm(frame, &confirm.prompt),
        Mode::Normal => {}
    }
}

/// 現在の端末幅から diff pane の実描画幅 (枠線を除く) を計算する。
/// `tui.rs` が `draw` より前に `App::sync_diff` を呼ぶ際、
/// `SHIKIGAMI_DIFF_FILTER` へ渡す `COLUMNS` を出すのに使う。`draw` 内の
/// レイアウトと同じ `Layout::horizontal` を通すことで、実際の描画幅と
/// ずれない。
pub fn diff_pane_width(total_width: u16) -> u16 {
    let [_, right_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(Rect::new(0, 0, total_width, 1));
    right_area.width.saturating_sub(2) // 左右の枠線
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title);
    if focused {
        // focus は border 色で示す。title に `*` を付ける方式だと
        // 幅が変わって pane がガタつく。
        block.border_style(Style::default().fg(Color::Cyan))
    } else {
        block.border_style(Style::default().fg(Color::DarkGray))
    }
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    let marked = app.marked.as_ref().map(|c| c.id.as_str());
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| match row {
            Row::Connector(graph) => ListItem::new(Line::from(Span::styled(
                graph.clone(),
                Style::default().fg(Color::DarkGray),
            ))),
            Row::Change { graph, change } => {
                let mut spans = vec![
                    // mark 列。squash/rebase の相手がどれかは常に見えて
                    // いないと危ないので、1 桁固定で常に確保する。
                    Span::styled(
                        if marked == Some(change.id.as_str()) {
                            "◆"
                        } else {
                            " "
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(graph.clone(), Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        change.short_id.clone(),
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(change.commit_id.clone(), Style::default().fg(Color::Blue)),
                    Span::raw(" "),
                ];
                if !change.bookmarks.is_empty() {
                    spans.push(Span::styled(
                        change.bookmarks.join(" "),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::raw(" "));
                }
                for (flag, color) in [
                    (change.has_conflict.then_some("conflict"), Color::Red),
                    (change.is_empty.then_some("empty"), Color::DarkGray),
                    (change.is_immutable.then_some("immutable"), Color::Yellow),
                ] {
                    if let Some(flag) = flag {
                        spans.push(Span::styled(
                            format!("({flag}) "),
                            Style::default().fg(color),
                        ));
                    }
                }
                let desc = if change.description.is_empty() {
                    Span::styled(
                        change.summary(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )
                } else {
                    Span::raw(change.description.clone())
                };
                spans.push(desc);
                ListItem::new(Line::from(spans))
            }
        })
        .collect();

    let title = match &app.revset {
        Some(revset) => format!(" log  -r {revset} "),
        None => " log ".to_string(),
    };
    let list = List::new(items)
        .block(pane_block(title, app.focus == Focus::Log))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    state.select(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn diff_file_color(status: char) -> Color {
    match status {
        'A' => Color::Green,
        'D' => Color::Red,
        'M' => Color::Yellow,
        'R' | 'C' => Color::Blue,
        _ => Color::White,
    }
}

fn draw_files(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .files
        .iter()
        .map(|f: &DiffFile| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", f.status),
                    Style::default()
                        .fg(diff_file_color(f.status))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(f.path.clone()),
            ]))
        })
        .collect();

    let title = format!(" files ({}) ", app.files.len());
    let list = List::new(items)
        .block(pane_block(title, app.focus == Focus::Files))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    if !app.files.is_empty() {
        state.select(Some(app.file_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_diff(frame: &mut Frame, area: Rect, app: &App) {
    let title = match (app.selected_change(), app.selected_file()) {
        (Some(change), Some(file)) => {
            format!(" {} {} — {} ", change.short_id, change.commit_id, file.path)
        }
        (Some(change), None) => format!(" {} {} ", change.short_id, change.commit_id),
        (None, _) => " diff ".to_string(),
    };
    // wrap しない: diff は桁が意味を持つ (`+`/`-` の列、インデント)。
    // 折り返すと行番号と scroll 位置の対応も崩れる。
    let paragraph = Paragraph::new(app.diff.clone())
        .block(pane_block(title, app.focus == Focus::Diff))
        .scroll((app.diff_scroll, 0));
    frame.render_widget(paragraph, area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let style = match app.status.kind {
        StatusKind::Info => Style::default().fg(Color::Black).bg(Color::Cyan),
        StatusKind::Error => Style::default().fg(Color::White).bg(Color::Red),
    };
    let left = Span::styled(format!(" {} ", app.status.text), style);
    let right = Span::styled(
        format!(
            " @ {}  mark:{}  rebase:{} ",
            app.working_copy,
            app.marked
                .as_ref()
                .map(|c| c.short_id.as_str())
                .unwrap_or("-"),
            app.rebase_mode.flag(),
        ),
        Style::default().fg(Color::DarkGray),
    );
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(right.width() as u16),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(Line::from(left)), left_area);
    frame.render_widget(Paragraph::new(Line::from(right)), right_area);
}

/// 画面中央に `width` x `height` の箱を取る。
fn centered(frame: &Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn draw_help(frame: &mut Frame) {
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(keys, desc)| {
            Line::from(vec![
                Span::styled(
                    format!("{keys:>16}  "),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(*desc),
            ])
        })
        .collect();
    let area = centered(frame, 78, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" keys ")
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        area,
    );
}

fn draw_input(frame: &mut Frame, prompt: &str, value: &str, ai_available: bool) {
    let area = centered(frame, 72, 3);
    frame.render_widget(Clear, area);
    let hint = if ai_available { " / Ctrl-g: ai" } else { "" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(value.to_string()),
            // 端末の cursor は raw mode 中は隠しているので、入力位置は
            // ブロックカーソルを自分で描いて示す。
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]))
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(format!(" {prompt}  (Enter: ok / Esc: cancel{hint}) "))
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        area,
    );
}

fn draw_confirm(frame: &mut Frame, prompt: &str) {
    let area = centered(frame, 72, 4);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(prompt.to_string()),
            Line::from(Span::styled(
                "y / Enter: yes    any other key: cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" confirm ".bold())
                .border_style(Style::default().fg(Color::Red)),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::testutil::repo_or_skip;

    /// frame を 1 枚描いて、cell の中身を行ごとの文字列にする。
    /// PTY をキャプチャすると ratatui の差分描画 (cursor 移動) で
    /// 空白が落ちるため、実際の見た目の検証は TestBackend で行う。
    fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        app.sync_diff(diff_pane_width(width));
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn log_pane_keeps_the_graph_prefix_and_separates_the_columns() {
        let (_tmp, mut app) = repo_or_skip!();
        let lines = render(&mut app, 120, 20);
        let joined = lines.join("\n");

        // working copy 行は jj の graph をそのまま持つ。これが落ちると
        // 枝が繋がって見えなくなる。
        let wc = lines
            .iter()
            .find(|l| l.contains("third change"))
            .unwrap_or_else(|| panic!("working copy row missing:\n{joined}"));
        assert!(wc.contains("@"), "graph prefix missing: {wc}");
        // change id / commit id / description が空白で区切られている
        // (span を詰めると `qxymuzsq5e3b8962first change` になる)。
        let columns: Vec<&str> = wc.split_whitespace().collect();
        assert!(
            columns.len() >= 4,
            "columns are not separated: {wc} -> {columns:?}"
        );
        // revset は pane title に出る。
        assert!(
            joined.contains("-r all()"),
            "revset title missing:\n{joined}"
        );
    }

    #[test]
    fn status_bar_reports_mark_and_rebase_mode() {
        let (_tmp, mut app) = repo_or_skip!();
        assert!(render(&mut app, 120, 20).last().unwrap().contains("mark:-"));

        app.handle_key(key(KeyCode::Char('m')));
        app.handle_key(key(KeyCode::Char('R')));
        let marked = app.marked.as_ref().unwrap().short_id.clone();
        let status = render(&mut app, 120, 20).last().unwrap().clone();
        assert!(status.contains(&format!("mark:{marked}")), "{status}");
        // `R` で -r → -s。破壊的操作の効き方が変わるので、常時表示する。
        assert!(status.contains("rebase:-s"), "{status}");
    }

    #[test]
    fn marked_row_is_flagged_in_the_log() {
        let (_tmp, mut app) = repo_or_skip!();
        app.handle_key(key(KeyCode::Char('m')));
        let marked = app.marked.as_ref().unwrap().short_id.clone();
        let lines = render(&mut app, 120, 20);
        // 1 行目は pane の枠 (diff 側の title にも同じ short id が出る)
        // なので、log pane の本体行だけを見る。行頭は縦罫 → mark 列。
        let row = lines
            .iter()
            .find(|l| l.starts_with('│') && l.contains(&marked))
            .unwrap_or_else(|| panic!("marked row vanished:\n{}", lines.join("\n")));
        assert_eq!(
            row.chars().nth(1),
            Some('◆'),
            "mark column not drawn: {row}"
        );
    }

    #[test]
    fn confirm_overlay_shows_the_change_it_will_destroy() {
        let (_tmp, mut app) = repo_or_skip!();
        app.handle_key(key(KeyCode::Char('a')));
        let joined = render(&mut app, 120, 20).join("\n");
        assert!(joined.contains("confirm"), "{joined}");
        // 何を消すのかが overlay に出ていること (short id と description)。
        assert!(joined.contains("third change"), "{joined}");
        assert!(joined.contains("y / Enter: yes"), "{joined}");
    }

    #[test]
    fn help_overlay_lists_every_documented_key() {
        let (_tmp, mut app) = repo_or_skip!();
        app.handle_key(key(KeyCode::Char('?')));
        // 画面を十分大きくして全行出す。KEYS が増えたとき help の箱が
        // 足りなくなるのをここで検知する。
        let joined = render(&mut app, 120, KEYS.len() as u16 + 4).join("\n");
        for (keys, _) in KEYS {
            assert!(
                joined.contains(keys.trim()),
                "help overlay is missing `{keys}`:\n{joined}"
            );
        }
    }

    #[test]
    fn diff_pane_titles_the_selected_change() {
        let (_tmp, mut app) = repo_or_skip!();
        let selected = app.selected_change().unwrap().clone();
        let joined = render(&mut app, 140, 24).join("\n");
        assert!(
            joined.contains(&selected.short_id) && joined.contains(&selected.commit_id),
            "diff pane title missing ids:\n{joined}"
        );
    }

    #[test]
    fn input_overlay_hints_ai_only_when_the_prompt_supports_it() {
        let (_tmp, mut app) = repo_or_skip!();
        // describe: diff を持つので Ctrl-g のヒントが出る。
        app.handle_key(key(KeyCode::Char('e')));
        let joined = render(&mut app, 120, 20).join("\n");
        assert!(joined.contains("Ctrl-g: ai"), "{joined}");
        app.handle_key(key(KeyCode::Esc));

        // bookmark: diff と無関係なのでヒントは出ない。
        app.handle_key(key(KeyCode::Char('b')));
        let joined = render(&mut app, 120, 20).join("\n");
        assert!(!joined.contains("Ctrl-g"), "{joined}");
    }
}
