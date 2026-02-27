use std::collections::HashMap;
use std::fs;

/// Known AI tool process names
const AI_TOOLS: &[(&str, &str)] = &[
    ("claude", "Claude"),
    ("codex", "Codex"),
    ("aider", "Aider"),
    ("copilot", "Copilot"),
    ("cursor", "Cursor"),
];

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AiStatus {
    Running { tool: String, pid: u32 },
    Idle { tool: String, pid: u32 },
    Finished { tool: String },
}

#[allow(dead_code)]
impl AiStatus {
    pub fn symbol(&self) -> &'static str {
        match self {
            AiStatus::Running { .. } => "●",
            AiStatus::Idle { .. } => "◐",
            AiStatus::Finished { .. } => "○",
        }
    }

    pub fn tool_name(&self) -> &str {
        match self {
            AiStatus::Running { tool, .. }
            | AiStatus::Idle { tool, .. }
            | AiStatus::Finished { tool, .. } => tool,
        }
    }
}

/// Detect AI tool processes among descendants of the given PID.
/// Returns the "most interesting" status (Running > Idle > Finished).
pub fn detect(child_pid: u32, prev_status: Option<&AiStatus>) -> Option<AiStatus> {
    let descendants = get_descendant_pids(child_pid);

    let mut best: Option<AiStatus> = None;

    for pid in &descendants {
        let comm = match read_comm(*pid) {
            Some(c) => c,
            None => continue,
        };

        for &(pattern, display_name) in AI_TOOLS {
            if comm.contains(pattern) {
                let state = read_process_state(*pid);
                let status = match state {
                    Some('R') => AiStatus::Running {
                        tool: display_name.to_string(),
                        pid: *pid,
                    },
                    _ => AiStatus::Idle {
                        tool: display_name.to_string(),
                        pid: *pid,
                    },
                };
                // Prefer Running over Idle
                match (&best, &status) {
                    (None, _) => best = Some(status),
                    (Some(AiStatus::Idle { .. }), AiStatus::Running { .. }) => {
                        best = Some(status)
                    }
                    _ => {}
                }
            }
        }
    }

    // If we had a previous AI status but the process is gone, mark as Finished
    if best.is_none() {
        if let Some(prev) = prev_status {
            match prev {
                AiStatus::Running { tool, .. } | AiStatus::Idle { tool, .. } => {
                    return Some(AiStatus::Finished {
                        tool: tool.clone(),
                    });
                }
                AiStatus::Finished { .. } => return Some(prev.clone()),
            }
        }
    }

    best
}

/// Get all descendant PIDs of a process by walking /proc
fn get_descendant_pids(root_pid: u32) -> Vec<u32> {
    // Build parent->children map from /proc
    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();

    let proc_dir = match fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let pid: u32 = match name_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let stat_path = format!("/proc/{}/stat", pid);
        if let Some(ppid) = read_ppid(&stat_path) {
            children_map.entry(ppid).or_default().push(pid);
        }
    }

    // BFS from root_pid
    let mut result = Vec::new();
    let mut queue = vec![root_pid];
    while let Some(pid) = queue.pop() {
        if let Some(kids) = children_map.get(&pid) {
            for &kid in kids {
                result.push(kid);
                queue.push(kid);
            }
        }
    }
    result
}

/// Read the comm (process name) for a PID
fn read_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{}/comm", pid))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Read process state character (R, S, D, Z, T, etc.)
fn read_process_state(pid: u32) -> Option<char> {
    let status = fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("State:") {
            return rest.trim().chars().next();
        }
    }
    None
}

/// Read PPID from /proc/{pid}/stat
fn read_ppid(stat_path: &str) -> Option<u32> {
    let content = fs::read_to_string(stat_path).ok()?;
    // Format: pid (comm) state ppid ...
    // comm can contain spaces and parens, so find last ')' then parse
    let after_comm = content.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // fields[0] = state, fields[1] = ppid
    fields.get(1)?.parse().ok()
}
