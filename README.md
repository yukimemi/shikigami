<!-- The `#gh-light-mode-only` / `#gh-dark-mode-only` fragment trick is
     GitHub-only, and on crates.io it also breaks the image outright:
     crates.io rewrites a plain relative `assets/logo.svg` to
     `github.com/<repo>/raw/HEAD/assets/logo.svg?sanitize=true` (verified
     on rvpm 3.50.0, whose logo renders fine), but with a fragment
     appended it stops at `/blob/HEAD/...#gh-light-mode-only` — an HTML
     page, not an image. Absolute raw.githubusercontent URLs inside
     <picture> sidestep both: GitHub honours prefers-color-scheme, and
     crates.io falls back to the <img>. -->
<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/shikigami/main/assets/logo-dark.svg">
    <img src="https://raw.githubusercontent.com/yukimemi/shikigami/main/assets/logo.svg" width="560" alt="shikigami">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/yukimemi/shikigami/actions/workflows/ci.yml"><img src="https://github.com/yukimemi/shikigami/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/yukimemi/shikigami/actions/workflows/jj.yml"><img src="https://github.com/yukimemi/shikigami/actions/workflows/jj.yml/badge.svg" alt="jj tests"></a>
  <a href="https://crates.io/crates/shikigami"><img src="https://img.shields.io/crates/v/shikigami.svg" alt="crates.io"></a>
  <a href="https://github.com/yukimemi/shikigami/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT"></a>
</p>

<p align="center">
  A terminal UI for <a href="https://github.com/jj-vcs/jj">Jujutsu (jj)</a>: read the change graph
  with its diffs, and run <code>new</code> / <code>edit</code> / <code>describe</code> /
  <code>squash</code> / <code>rebase</code> / <code>abandon</code> / <code>undo</code> without
  leaving the log.
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/yukimemi/shikigami/main/vhs/demo.gif" width="900" alt="shikigami demo: browsing the jj change graph, describing a change, squashing into a marked target, then undoing it">
</p>

<p align="center">
  <sub>Regenerate with <code>cargo make vhs-regen</code> — the tape builds its own throwaway jj repo, so the GIF never depends on a working copy.</sub>
</p>

## Why

`jj log` is already the best part of jj — but acting on what you see means retyping change ids into
another command. shikigami keeps the log on screen, shows the selected change's diff next to it, and
binds the rewrite commands to single keys. Every key press is a real `jj` invocation, so your
`~/.config/jj/config.toml` (`revsets.log`, `immutable_heads()`, diff formatter, colors) applies
unchanged, and `jj op log` / `jj undo` still see everything shikigami did.

## Install

```sh
cargo install shikigami
```

Requires the `jj` binary on `PATH` (shikigami drives the CLI; it does not link `jj-lib`).

Editor integration: [shikigami.nvim](https://github.com/yukimemi/shikigami.nvim) opens shikigami in
a Neovim terminal and reloads buffers that jj rewrote underneath you.

## Usage

```sh
shikigami                      # TUI for the repo containing the cwd
shikigami -R /path/to/repo     # ... for another repo
shikigami -r 'all()'           # ... with an explicit revset
shikigami completion zsh       # shell completion
shikigami self-update          # update the shikigami binary itself
```

Without `-r`, shikigami uses jj's own `revsets.log`, i.e. the same set of changes `jj log` shows.

`self-update` fetches the latest GitHub release via [kaishin](https://github.com/yukimemi/kaishin)
and replaces the running binary — skip the confirmation prompt with `-y`/`--yes`, or check
availability only with `--check`. It detects a `cargo install`-managed binary and prefers
downloading the matching release asset over a source rebuild; a dev build under `target/` is left
alone.

## Keys

| Key               | Action                                                        |
| ----------------- | ------------------------------------------------------------- |
| `j` / `k`         | move (log pane), select a file (files pane), or scroll (diff pane) |
| `g` / `G`         | first / last (in whichever pane has focus)                    |
| `Ctrl-d`/`Ctrl-u` | half-page move                                                |
| `Tab` / `Shift-Tab` | cycle focus: log → files → diff (`Shift-Tab` reverses)   |
| `Enter`           | `jj edit` — make the selected change the working copy         |
| `n`               | `jj new` — child of the selected change (description prompt)  |
| `e`               | `jj describe` — edit the description                          |
| Ctrl-g            | (in the `n`/`e` prompt) fill the message from the diff via AI |
| `r`               | `jj restore` — discard the selected file's change (files pane)|
| `p`               | `jj absorb` — move the selected file's change into ancestor commits (files pane) |
| `o`               | open the selected file in `$EDITOR` (or `$VISUAL`) (files pane) |
| `b`               | `jj bookmark set`                                             |
| `m`               | mark the selected change as the squash / rebase target (`◆`)  |
| `s`               | `jj squash` — selected change into the marked one             |
| `x`               | `jj rebase` — selected onto the marked change, or onto a revset you type |
| `R`               | cycle rebase mode: `-r` / `-s` / `-b`                         |
| `L`               | cycle pane layout: `stacked` (log \| files-over-diff) / `diff-below` (log+files on top, full-width diff below) |
| `a`               | `jj abandon`                                                  |
| `u` / `U`         | `jj undo` / `jj redo`                                         |
| `/`               | set the log revset                                            |
| `A`               | toggle the revset `all()`                                     |
| `Ctrl-r`          | reload                                                        |
| `?`               | toggle help                                                   |
| `q`, `Esc`        | quit                                                          |

Everything that rewrites history asks first (`y` / `Enter` confirms; **any** other key cancels), and
the status bar always shows the working copy, the marked target, and the current rebase mode.

## AI-generated messages

Inside the `n` (`jj new`) and `e` (`jj describe`) description prompt, `Ctrl-g` fills the input
with a message generated from the selected change's diff. shikigami does not talk to any AI
vendor itself: set `SHIKIGAMI_AI_CMD` to a shell command that reads a diff on stdin and prints a
message on stdout, e.g.

```sh
export SHIKIGAMI_AI_CMD='opencode run -m sonnet "write a one-line commit message for this diff:" -'
```

Any CLI that can read a diff on stdin works (`aichat`, `sgpt`, `mods`, your own script, …). The
output is folded to a single line; the prompt only ever holds one. Without `SHIKIGAMI_AI_CMD` set,
`Ctrl-g` reports the missing configuration in the status bar instead of doing nothing silently.

**If your `SHIKIGAMI_AI_CMD` is an agentic coding CLI (Claude Code, opencode, Codex, …)
rather than a plain completion tool, disable its tool/file-edit access.** The diff is untrusted
content (it can contain anything a contributor typed) piped straight into that command; an
agent with tool access may try to *act* on a diff instead of just describing it — up to and
including editing files in whatever directory it's launched from. E.g. for Claude Code, add
`--allowedTools ""` to deny every tool; running with tools denied is also markedly more reliable
for this one-line-summary use case than leaving them enabled.

## Nicer diffs (difftastic, delta, …)

shikigami never picks a diff format itself — `jj diff` decides, so two knobs from your existing jj
setup carry straight through:

- **`ui.diff-formatter`** (jj-side). shikigami no longer forces `--git`, so if you've configured an
  external diff generator (e.g. [difftastic](https://difftastic.wilfred.me.uk/), which diffs trees
  directly and syntax-highlights the result), the diff pane shows exactly that — same as running
  `jj diff` in your shell.
- **`SHIKIGAMI_DIFF_FILTER`** (shikigami-side). A shell command that receives the diff (ANSI
  included) on stdin and must print ANSI on stdout; useful for pager-style tools like
  [delta](https://github.com/dandavison/delta), which shikigami never invokes on its own since it
  always runs jj with `--no-pager`:

  ```sh
  export SHIKIGAMI_DIFF_FILTER='delta --paging=never --side-by-side --width "$COLUMNS"'
  ```

  The diff pane's actual rendered width is passed to the command as `COLUMNS`, so a wrapper that
  reads it (as above) gets side-by-side output that fits the pane instead of wrapping at a fixed
  80 columns. Since the default layout only gives the diff pane half the terminal width, press `L`
  to switch to the `diff-below` layout (log/files on top, full-width diff below) for more room —
  see "Keys" above. If the command fails (missing binary, bad flags, non-zero exit), the diff pane
  falls back to jj's raw output and the error shows in the status bar — a broken filter never
  blanks the diff.

## Opening files in your editor

Press `o` in the files pane to open the currently selected file in `$EDITOR` (falling back to
`$VISUAL` if `$EDITOR` is unset). shikigami leaves the alternate screen for the duration — so
full-screen editors (`vim`, `nvim`, `emacs -nw`) work normally — and redraws once the editor exits.
Renamed/copied files open their new path; if neither `$EDITOR` nor `$VISUAL` is set, `o` reports
that in the status bar instead of doing nothing.

`$EDITOR`/`$VISUAL` is split on whitespace (no shell, no quoting) before the file path is appended,
e.g. `EDITOR='code --wait'` becomes `code --wait <path>`. This is the one place shikigami hands the
terminal to another interactive program on purpose — contrast with "No editor surprises" below,
which is about jj's own *implicit* editor invocation (`ui.editor`), always blocked.

## Design notes

- **jj CLI, not `jj-lib`.** `jj-lib` has no semver guarantee; the CLI's template language and flags
  are the stable, public interface — and going through the CLI means user config just works.
- **Graph comes from jj.** The log template starts with an ASCII record separator, so the text
  before it is jj's own graph drawing (`│ ○`, `├─╮`). shikigami never re-derives topology.
- **No editor surprises.** Mutating commands always pass `-m` / `--use-destination-message`, and are
  additionally run with `ui.editor` pointed at a non-existent program: if jj ever wants an editor it
  fails visibly in the status bar instead of hijacking the alternate screen. This only blocks jj's
  own *implicit* invocation — `o` in the files pane still opens `$EDITOR` on purpose (see
  "Opening files in your editor" above).
- **Diff colors are jj's (optionally filtered).** `jj diff --color always` output is converted to
  styled terminal text as-is, and the format is whatever `ui.diff-formatter` resolves to — no
  `--git` is forced. `SHIKIGAMI_DIFF_FILTER` (see "Nicer diffs" above) can post-process that ANSI
  through an external pager-style tool; shikigami never re-derives or re-highlights a diff itself.
- **Files pane, one file at a time.** The right column is split into a file list
  (`jj diff --summary`) above the diff. The diff pane only ever shows the file currently selected
  there — like lazygit — instead of the whole change's diff scrolling past all at once.

## Caching

jj's object store is content-addressed: a `commit_id` is derived from the commit's own content, and
rewrites (`describe`/`squash`/`rebase`/`abandon`/…) always mint a new id rather than mutating an
existing one. shikigami relies on that to cache `jj diff`/`jj diff --summary`/`jj show` output per
`commit_id`, both in memory for the running session and on disk across restarts — revisiting a
commit you've already looked at (even in a previous run) never re-invokes `jj`. The disk cache lives
under the OS cache directory (`shikigami/<repo-hash>.json`) and keeps at most the 300
most-recently-used commits per repo; override the location with `SHIKIGAMI_CACHE_DIR`.

Startup itself never blocks on `jj`: the alternate screen opens immediately, and the initial
`jj log`/`jj status` run on a background thread while the log pane shows "loading…" — this matters
most on Windows, where spawning a process is noticeably slower than on Unix.

## Development

```sh
cargo make check      # editorconfig + fmt + clippy + tests + lock check (same as CI)
cargo make test
```

Tests create real jj repos in temp dirs and drive the state machine with synthetic key events, so
`jj` must be installed to run them.

## License

MIT
