# zmux

A replacement for tmux with specializations for cli-based AI tools, like Claude Code or Codex.

## Keybindings

### Normal Mode

| Key | Action |
|-----|--------|
| `Ctrl+B` | Enter nav mode |

### Nav Mode

| Key | Action |
|-----|--------|
| `Esc` / `Enter` | Return to normal mode |
| `d` | Detach from session |
| `k` / `Up` | Move focus up (Window → Group → Project) |
| `j` / `Down` | Move focus down (Project → Group → Window) |
| `h` / `Left` | Previous tab at current level |
| `l` / `Right` | Next tab at current level |
| `1`-`9` | Select tab by index |
| `x` | Close active window |
| `c` | Create new window |
| `g` | Move current window to new group (named after cwd) |
| `p` | Move current window to new project (named after cwd) |
