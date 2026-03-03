use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Check if a directory is inside a git repository.
pub fn is_git_repo(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a git worktree under `<project_dir>/.worktrees/<branch>`.
/// Creates the branch if it doesn't exist.
/// Returns the worktree path.
pub fn create(project_dir: &Path, branch: &str) -> Result<PathBuf> {
    let worktrees_dir = project_dir.join(".worktrees");
    std::fs::create_dir_all(&worktrees_dir)
        .with_context(|| format!("Failed to create .worktrees dir in {}", project_dir.display()))?;

    let wt_path = worktrees_dir.join(branch);

    if wt_path.exists() {
        // Worktree already exists, just return the path
        return Ok(wt_path);
    }

    // Try creating with a new branch first
    let output = Command::new("git")
        .args(["worktree", "add", "-b", branch, &wt_path.to_string_lossy()])
        .current_dir(project_dir)
        .output()
        .with_context(|| "Failed to run git worktree add")?;

    if !output.status.success() {
        // Branch might already exist, try without -b
        let output2 = Command::new("git")
            .args(["worktree", "add", &wt_path.to_string_lossy(), branch])
            .current_dir(project_dir)
            .output()
            .with_context(|| "Failed to run git worktree add")?;

        if !output2.status.success() {
            let stderr = String::from_utf8_lossy(&output2.stderr);
            anyhow::bail!("git worktree add failed: {}", stderr.trim());
        }
    }

    Ok(wt_path)
}

/// Check if a worktree has uncommitted changes.
pub fn is_dirty(wt_path: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wt_path)
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Remove a git worktree. Use force=true to remove even with uncommitted changes.
pub fn remove(project_dir: &Path, wt_path: &Path, force: bool) -> Result<()> {
    let wt_str = wt_path.to_string_lossy();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&wt_str);

    let output = Command::new("git")
        .args(&args)
        .current_dir(project_dir)
        .output()
        .with_context(|| "Failed to run git worktree remove")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree remove failed: {}", stderr.trim());
    }

    Ok(())
}

/// List all local git branches in the repository.
pub fn list_branches(project_dir: &Path) -> Vec<String> {
    Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(project_dir)
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

