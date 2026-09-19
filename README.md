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
```

Without `-r`, shikigami uses jj's own `revsets.log`, i.e. the same set of changes `jj log` shows.

## Keys

| Key               | Action                                                        |
| ----------------- | ------------------------------------------------------------- |
| `j` / `k`         | move (log pane) or scroll (diff pane)                         |
| `g` / `G`         | first / last                                                  |
| `Ctrl-d`/`Ctrl-u` | half-page move                                                |
| `Tab`             | switch focus between the log and the diff                     |
| `Enter`           | `jj edit` — make the selected change the working copy         |
| `n`               | `jj new` — child of the selected change (description prompt)  |
| `e`               | `jj describe` — edit the description                          |
| `b`               | `jj bookmark set`                                             |
| `m`               | mark the selected change as the squash / rebase target (`◆`)  |
| `s`               | `jj squash` — selected change into the marked one             |
| `x`               | `jj rebase` — selected onto the marked change, or onto a revset you type |
| `R`               | cycle rebase mode: `-r` / `-s` / `-b`                         |
| `a`               | `jj abandon`                                                  |
| `u` / `U`         | `jj undo` / `jj redo`                                         |
| `/`               | set the log revset                                            |
| `A`               | toggle the revset `all()`                                     |
| `Ctrl-r`          | reload                                                        |
| `?`               | toggle help                                                   |
| `q`, `Esc`        | quit                                                          |

Everything that rewrites history asks first (`y` / `Enter` confirms; **any** other key cancels), and
the status bar always shows the working copy, the marked target, and the current rebase mode.

## Design notes

- **jj CLI, not `jj-lib`.** `jj-lib` has no semver guarantee; the CLI's template language and flags
  are the stable, public interface — and going through the CLI means user config just works.
- **Graph comes from jj.** The log template starts with an ASCII record separator, so the text
  before it is jj's own graph drawing (`│ ○`, `├─╮`). shikigami never re-derives topology.
- **No editor surprises.** Mutating commands always pass `-m` / `--use-destination-message`, and are
  additionally run with `ui.editor` pointed at a non-existent program: if jj ever wants an editor it
  fails visibly in the status bar instead of hijacking the alternate screen.
- **Diff colors are jj's.** `jj diff --color always` output is converted to styled terminal text, so
  the diff looks exactly as it does in your shell.

## Development

```sh
cargo make check      # editorconfig + fmt + clippy + tests + lock check (same as CI)
cargo make test
```

Tests create real jj repos in temp dirs and drive the state machine with synthetic key events, so
`jj` must be installed to run them.

## License

MIT
