# zmux Roadmap

## Completed

### Phase 1: Single-Window Terminal Emulator
- PTY spawning with `portable-pty`, rendering via `ratatui` + `tui-term`
- Full keyboard input forwarding, terminal resize handling

### Phase 2: Session Tree + Tab Navigation
- Hierarchical data model: projects > groups > windows
- Tab bar with breadcrumb navigation, nav mode (`Ctrl-B`), vim-style `hjkl`
- Horizontal tab scrolling for many windows

### Phase 3: Presets + Project Settings
- TOML preset files at `~/.config/zmux/presets/<name>.toml`
- `zmux <preset>` to launch, `zmux list` to list presets
- Save/set project and group directories from within zmux (`Ctrl-B s`/`S`)
- `.env` file auto-parsing and injection into PTY processes
- Groups inherit project settings unless overridden
- Save session tree back to preset (`Ctrl-B W`)

### Client-Server Architecture
- Server owns PTYs and session tree, persists across client disconnects
- Client connects over Unix socket, detach/reattach (`Ctrl-B d`)
- `zmux kill` to stop the server
- Auto-start server on `zmux` if not running
- Window auto-removal when shell process exits

---

## Phase 4: AI Process Detection

Detect running AI tools (Claude Code, Codex, Aider, etc.) in each window and surface their status.

### Implementation
- Poll `/proc` every 2-3 seconds for each window's PTY child process tree
- Walk descendant PIDs via `/proc/{pid}/task/*/children`, check `/proc/{pid}/comm` for known tool names (`claude`, `codex`, `aider`)
- Track status per window:
  - **Running** — AI process is active (R/S state, still producing output)
  - **Idle** — AI process is sleeping/waiting for user input
  - **Finished** — AI process has exited (capture exit code)
- Show colored status dots in the tab bar next to window names:
  - Green dot = running
  - Yellow dot = idle/waiting for input
  - Gray dot = finished
- Add an "all AI sessions" overview (nav mode command) that lists every window running an AI tool across all projects/groups, with status

### Files
- `src/ai_detect.rs` — process tree walking, tool identification, status tracking
- `src/server.rs` — periodic polling task, attach `AiStatus` to `WindowNode`
- `src/protocol.rs` — include AI status in `TabEntry` or `TabState`
- `src/ui.rs` — render status dots

---

## Phase 5: Git Worktree Integration

First-class git worktree support tied to the group lifecycle.

### Implementation
- Nav mode command to create a new group with an associated worktree:
  - Prompts for branch name (or auto-generates from group name)
  - Runs `git worktree add <path> -b <branch>` under the project directory
  - Stores worktree path as the group's `working_dir`
  - All windows in the group inherit the worktree as their cwd
- On group close/delete:
  - Check if worktree has uncommitted changes
  - If clean, auto-remove with `git worktree remove`
  - If dirty, warn user and require confirmation
- Preset support:
  - `worktree_branch = "feat/foo"` in group config
  - On preset load, auto-create worktree if it doesn't exist
- Detect whether the project directory is a git repo before offering worktree commands
- Worktrees stored under `<project_dir>/.worktrees/` by convention

### Files
- `src/worktree.rs` — create/remove worktrees via `git` subprocess, dirty check
- `src/server.rs` — hook into group creation/deletion lifecycle
- `src/config.rs` — `worktree_branch` field in `GroupPreset`
- `src/protocol.rs` — new `NewGroupWithWorktree` command (or extend `NewGroup`)

---

## Phase 6: Polish & Quality of Life

Ongoing improvements to make zmux a daily driver.

### Session Persistence
- Auto-save session tree to `~/.local/state/zmux/state.json` periodically
- On server restart, restore tree structure, directories, and window names
- PTY state is lost (processes are gone), but new shells spawn in the right directories

### Window Management
- Close/delete windows, groups, and projects
- Startup commands per window from presets (`command = "cargo watch"`)

### UX
- Mouse click on tab bar to switch tabs
- Scrollback / copy mode (scroll through terminal history with `Ctrl-B [`)
- Config file for keybindings, colors, default shell (`~/.config/zmux/config.toml`)
- Search across windows (find which window has a specific output)
