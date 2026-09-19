# shikigami demo — README GIF
#
# Usage:
#   Docker (recommended; native vhs hangs on `Set Theme` on Windows):
#     cd vhs && docker build -t shikigami-vhs . && bash regen.sh
#   Native (Linux/macOS):
#     cd vhs && vhs demo.tape     # needs jj + shikigami on PATH
#
# The tape builds its own throwaway jj repo under /tmp, so the GIF never
# depends on the author's working copy and never touches a real repo.
# Every mutation below happens inside that temp repo.
#
# Flow:
#   1. log + diff panes, cursor movement
#   2. Tab through the files pane into the diff pane and scroll
#   3. ? help overlay
#   4. e  → describe the selected change
#   5. n  → new child change
#   6. m + s → mark a destination and squash into it (confirm dialog)
#   7. u  → undo, putting the repo back
#   8. q

Output demo.gif
Set Shell "bash"
Set FontSize 16
Set FontFamily "PlemolJP Console NF"
Set Width 1400
Set Height 600
Set Padding 20
Set TypingSpeed 45ms
Set Theme "Catppuccin Mocha"

# ── fixture repo (hidden): three generations, a bookmark, real diffs ──
Hide
Type "rm -rf /tmp/demo && mkdir -p /tmp/demo && cd /tmp/demo && jj git init >/dev/null 2>&1"
Enter
Sleep 800ms
Type `printf 'fn main() {\n    println!("hello");\n}\n' > main.rs && jj describe -m 'feat: print a greeting' >/dev/null`
Enter
Sleep 600ms
Type "jj new -m 'feat: read the name from argv' >/dev/null"
Enter
Sleep 600ms
Type `printf 'fn main() {\n    let name = std::env::args().nth(1).unwrap_or("world".into());\n    println!("hello, {name}");\n}\n' > main.rs`
Enter
Sleep 400ms
Type "jj bookmark create main -r @- >/dev/null 2>&1"
Enter
Sleep 400ms
Type "jj new -m 'docs: mention the argument' >/dev/null"
Enter
Sleep 600ms
Type `printf '# demo\n\nPass a name as the first argument.\n' > README.md`
Enter
Sleep 400ms
Type "clear"
Enter
Sleep 200ms
Show

# ── 1. launch: log on the left, the selected change's diff on the right ──
Sleep 600ms
Type "shikigami"
Sleep 400ms
Enter
Sleep 2500ms

# move through the graph — the diff pane follows the cursor
Type "j"
Sleep 1200ms
Type "j"
Sleep 1200ms
Type "k"
Sleep 1000ms

# ── 2. files pane, then diff pane ──
Tab
Sleep 500ms
Tab
Sleep 800ms
Type "j"
Sleep 250ms
Type "j"
Sleep 250ms
Type "j"
Sleep 900ms
Type "g"
Sleep 600ms
Tab
Sleep 600ms

# ── 3. help ──
Type "?"
Sleep 4s
Escape
Sleep 600ms

# ── 4. describe: the prompt starts from the existing description ──
Type "e"
Sleep 1s
Type " and the greeting"
Sleep 1s
Enter
Sleep 2s

# ── 5. new child change ──
Type "n"
Sleep 800ms
Type "wip: try something"
Sleep 800ms
Enter
Sleep 2s

# ── 6. mark a destination, then squash the working copy into it ──
# `m` marks the change under the cursor (◆ in the left column); `s`
# squashes the selected change into it and asks for confirmation.
Type "j"
Sleep 800ms
Type "m"
Sleep 1200ms
Type "k"
Sleep 800ms
Type "s"
Sleep 2s
Type "y"
Sleep 2500ms

# ── 7. undo puts it back — jj keeps the whole operation log ──
Type "u"
Sleep 1500ms
Type "y"
Sleep 2500ms

# ── 8. quit ──
Type "q"
Sleep 1s
