# zmux

A replacement for tmux with specializations for cli-based AI tools, like Claude Code or Codex.

## Keybindings

### Mouse

| Action | Effect |
|--------|--------|
| Click project name | Open project picker dropdown |
| Click group name | Open group picker dropdown |
| Click window tab | Switch to clicked window |

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
| `W` | Open preset save view — tree of projects/groups/windows with checkboxes (Space to toggle, Enter to name/save the selection) |
| `L` | Load preset into current session (type to filter, ↑/↓ to select, Tab to autocomplete) |
| `w` | Create new worktree group (type a name or pick from branch list with ↑/↓, Tab to autocomplete) |
| `X` | Close active group (removes worktree if applicable) |
| `R` | Rebase active group's branch onto main |
| `M` | Merge active group's worktree branch into main |
| `/` | Open session tree in search mode (filter by project/group/window names and buffer content) |
| `[` | Enter copy (scroll) mode |
| `]` | Paste from copy buffer |
| `t` | Toggle layout mode (Stacked ↔ Tiled) |
| `v` | Split active pane vertically (side by side) |
| `-` | Split active pane horizontally (top/bottom) |
| `T` | Swap split direction (H↔V) at active pane |
| `m` | Close active pane (unsplit; window stays alive) |
| `n` / `N` | Cycle pane content to next/previous window in group |
| `o` / `O` | Cycle pane content globally (across all groups/projects) |
| `Ctrl+H/J/K/L` | Move focus between panes (tiled mode only) |
| `Shift+Arrow` | Resize active pane |
| `e` | Set env profile for focused tab level (type to filter, ↑/↓ to select) |
| `E` | Source env profile into active window's running shell |
| `u` | Hot reload server binary (upgrade in place) |
| `f` | Open session tree navigator (with preview) |
| `F` | Open swap-pane finder — pick any window to replace focused pane (tiled mode) |
| `?` | Show help overlay |

### Tree Nav Mode

| Key | Action |
|-----|--------|
| `q` / `Esc` | Close tree nav |
| `j` / `Down` | Move cursor down |
| `k` / `Up` | Move cursor up |
| `Space` | Toggle collapse/expand on projects and groups |
| `h` | Fold: collapse current item; on window, collapses parent group |
| `l` | Expand: expand current item and move to first child |
| `Enter` | Select item and jump to it |
| `H` | Collapse one level at a time (groups first, then projects) |
| `L` | Expand one level at a time (projects first, then groups) |
| `J` | Jump to next item of same level |
| `K` | Jump to previous item of same level |
| `r` | Rename focused item |
| `x` | Close/kill focused item (window, group, or project) |
| `c` | Send Ctrl-C (interrupt) to focused item (all windows if group/project) |
| `P` | Load preset (stays in tree nav, jumps to first new window) |
| `g` | Jump to top |
| `G` | Jump to bottom |
| `/` | Start incremental search filter (by project/group/window names, falling back to window buffer content) |
| Click item | Select item and jump to it |
| Scroll wheel | Move cursor up/down |

### Tree Select Mode (after pressing `W` in Nav — selective preset save)

| Key | Action |
|-----|--------|
| `Space` | Toggle the item under the cursor (cascades to descendants for projects/groups; partial-state parents snap to fully on) |
| `Enter` | Open name prompt for the selected items (pre-filled with current preset name) |
| `Esc` | Cancel without saving |
| `j` / `k` / `↑` / `↓` | Move cursor |
| `g` / `G` | Jump to top / bottom |
| `h` / `l` | Fold / expand |
| `H` / `L` | Collapse / expand one level at a time |
| `J` / `K` | Jump to next / previous item of same level |

Checkbox glyphs: `[x]` all selected, `[-]` partial, `[ ]` none. Groups with no selected windows and projects with no selected groups are dropped from the saved preset.

### Tree Search (after pressing `/` in Tree Nav)

| Key | Action |
|-----|--------|
| Type characters | Append to query; tree filters live; cursor jumps to first matching window |
| `Backspace` | Remove last character from query |
| `Up` / `Ctrl+K` | Move cursor up through filtered items |
| `Down` / `Ctrl+J` | Move cursor down through filtered items |
| `Enter` | Jump to focused item and exit |
| `Esc` | Cancel search (clears query, stays in tree nav) |

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
| `PageUp` | Scroll up full page |
| `PageDown` | Scroll down full page |
| `Mouse ScrollUp` | Scroll up (enters copy mode automatically) |
| `Mouse ScrollDown` | Scroll down (exits copy mode at bottom) |
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
| `zmux reload` | Hot reload the server with the current binary |
| `zmux kill` | Shut down the server |

## Presets

Presets are TOML files that define the session tree structure. They are stored in:

- **macOS**: `~/Library/Application Support/zmux/presets/`
- **Linux**: `~/.config/zmux/presets/`

```toml
[[project]]
name = "myproject"
path = "/home/user/myproject"

[[project.group]]
name = "default"

[[project.group.window]]
name = "editor"
command = "vim"

[[project.group.window]]
name = "dev-server"
path = "/home/user/myproject/frontend"
command = "task web:dev"

[[project.group]]
name = "feature-branch"
worktree_branch = "feature-branch"

[[project.group.window]]
name = "shell"
```

### Layout Presets

Groups can specify a `layout` to automatically arrange windows in a tiled layout:

```toml
[[project.group]]
name = "dev"
layout = "main-vertical"
```

Available layouts:

| Layout | Description |
|--------|-------------|
| `even-horizontal` | All panes side by side, equal width |
| `even-vertical` | All panes stacked, equal height |
| `main-horizontal` | One large pane on top, rest in a row below |
| `main-vertical` | One large pane on left, rest stacked on right |
| `tiled` | Grid layout (balanced rows and columns) |

### Startup Focus

Control which window/group gets focus when a preset loads:

```toml
[[project]]
name = "myproject"
path = "/home/user/myproject"
startup_group = "dev"
startup_window = "editor"
```

### Lifecycle Hooks

Hooks run shell commands at specific points in the session lifecycle:

```toml
[[project]]
name = "myproject"
path = "/home/user/myproject"
on_project_start = "git fetch --all"
on_project_first_start = "npm install"
on_project_restart = "git pull"
on_project_stop = "docker-compose down"
pre_window = "source .venv/bin/activate"
```

| Hook | When it runs | How it runs |
|------|-------------|-------------|
| `on_project_start` | Every time the preset loads | Subprocess (before windows) |
| `on_project_first_start` | Only the very first time | Subprocess (before windows) |
| `on_project_restart` | On subsequent loads | Subprocess (before windows) |
| `on_project_stop` | When project is closed or server shuts down | Subprocess |
| `pre_window` | Before each window's command | PTY keystrokes (affects the shell) |

Per-window hooks are also available:

```toml
[[project.group.window]]
name = "server"
command = "npm start"
on_pane_open = "echo 'server starting'"
on_pane_close = "echo 'server stopped' >> /tmp/log"
```

| Hook | When it runs | How it runs |
|------|-------------|-------------|
| `on_pane_open` | After the window command | PTY keystrokes |
| `on_pane_close` | When the window/pane is destroyed | Subprocess |

**Execution order** for a window: `pre_window` -> window `command` -> `on_pane_open`

## Env Profiles

Env profiles are `.env` files stored in:

- **macOS**: `~/Library/Application Support/zmux/envs/`
- **Linux**: `~/.config/zmux/envs/`

Create a profile by adding a `.env` file (e.g., `prod.env`):

```
DATABASE_URL=postgres://prod-host/mydb
API_KEY=secret123
```

**Setting profiles (new windows)**: In nav mode, press `e` to set an env profile for the focused tab level (project/group/window). New windows spawned in that scope will inherit the profile's variables at process startup (invisible to the shell).

**Sourcing into running shells**: Press `E` to source a profile into the active window's running shell via `set -a; source <path>; set +a`. This will be visible in the terminal.

**Preset support**: Presets can specify a default env profile at any level:

```toml
[[project]]
name = "myapp"
path = "/home/user/myapp"
env_profile = "dev"

[[project.group]]
name = "staging"
env_profile = "staging"
```

Layering order: directory `.env` files < project profile < group profile < window profile.

## Features

- **Hierarchical sessions**: Projects > Groups > Windows
- **AI awareness**: Detects claude, codex, aider, copilot processes and shows status indicators
- **Git worktree integration**: Create groups backed by git worktrees, rebase/merge from within zmux
- **Vim-style splits**: Binary split tree for tiling — split any pane horizontally or vertically, resize with ratios
- **Presets**: Save and restore session trees as TOML with layout presets, startup focus, and lifecycle hooks
- **.env support**: Auto-injects `.env` variables into new windows based on project/group directory
- **Client-server architecture**: Sessions persist across disconnects
