# zmux

A replacement for tmux with specializations for cli-based AI tools, like Claude Code or Codex.

## Keybindings

### Mouse

| Action | Effect |
|--------|--------|
| Click tab bar | Switch to clicked project/group/window |

### Normal Mode

| Key | Action |
|-----|--------|
| `Ctrl+B` | Enter nav mode |
| `Ctrl+Q` | Quit |
| `Ctrl+H/J/K/L` | Move focus between panes (tiled mode only) |

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
| `r` | Rename focused tab (project/group/window) |
| `x` | Close active window |
| `c` | Create new window |
| `g` | Move current window to new group (named after cwd) |
| `p` | Move current window to new project (named after cwd) |
| `a` | Enter AI nav mode |
| `s` | Save current cwd as group directory |
| `S` | Save current cwd as project directory |
| `W` | Save session as preset |
| `L` | Load preset into current session (type to filter, ↑/↓ to select, Tab to autocomplete) |
| `w` | Create new worktree group (type a name or pick from branch list with ↑/↓, Tab to autocomplete) |
| `X` | Close active group (removes worktree if applicable) |
| `R` | Rebase active group's branch onto main |
| `M` | Merge active group's worktree branch into main |
| `/` | Search across all windows for text |
| `[` | Enter copy (scroll) mode |
| `]` | Paste from copy buffer |
| `t` | Toggle layout mode (Stacked ↔ Tiled) |
| `T` | Cycle tile layout (columns → rows → main-left → grid) |
| `m` | Toggle current window in/out of tile set |
| `?` | Show help overlay |

### Copy Mode

| Key | Action |
|-----|--------|
| `q` / `Esc` | Exit copy mode (or cancel selection) |
| `h` / `Left` | Move cursor left |
| `l` / `Right` | Move cursor right |
| `k` / `Up` | Move cursor up (scrolls if at top) |
| `j` / `Down` | Move cursor down (scrolls if at bottom) |
| `0` | Move cursor to beginning of line |
| `$` | Move cursor to end of line |
| `w` | Jump to next word |
| `b` | Jump to previous word |
| `Ctrl+U` | Scroll up half page |
| `Ctrl+D` | Scroll down half page |
| `g` | Jump to top of scrollback |
| `G` | Jump to bottom (live view) |
| `Space` | Start/cancel selection |
| `Enter` | Yank selection and exit copy mode |

### AI Nav Mode

| Key | Action |
|-----|--------|
| `Esc` / `Enter` | Return to normal mode |
| `h` / `Left` | Previous AI window |
| `l` / `Right` / `a` | Next AI window |

## CLI

| Command | Action |
|---------|--------|
| `zmux` | Connect to server (starts one if needed) |
| `zmux <preset>` | Connect, starting server with preset if needed |
| `zmux server [preset]` | Run server in foreground |
| `zmux list` | List available presets |
| `zmux kill` | Shut down the server |

## Presets

Presets are TOML files stored in `~/.config/zmux/presets/`. They define the session tree structure:

```toml
[[project]]
name = "myproject"
path = "/home/user/myproject"

[[project.group]]
name = "default"

[[project.group.window]]
name = "editor"

[[project.group]]
name = "feature-branch"
worktree_branch = "feature-branch"

[[project.group.window]]
name = "shell"
```

## Features

- **Hierarchical sessions**: Projects > Groups > Windows
- **AI awareness**: Detects claude, codex, aider, copilot processes and shows status indicators
- **Git worktree integration**: Create groups backed by git worktrees, rebase/merge from within zmux
- **Tiling layouts**: i3-style tiled mode with columns, rows, main-left, and grid layouts
- **Presets**: Save and restore session trees as TOML
- **.env support**: Auto-injects `.env` variables into new windows based on project/group directory
- **Client-server architecture**: Sessions persist across disconnects
