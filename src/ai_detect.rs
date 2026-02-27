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
/// Uses CPU time delta to determine Running vs Idle:
/// - If total CPU time (utime+stime) across the AI tool's subtree increased since
///   last poll, it's Running. Otherwise it's Idle.
/// Returns (status, current_cpu_time) where cpu_time can be stored for next poll.
pub fn detect(child_pid: u32, prev_status: Option<&AiStatus>, prev_cpu_time: u64) -> (Option<AiStatus>, u64) {
    let descendants = get_descendant_pids(child_pid);

    // Find the AI tool process among descendants
    let mut ai_pid: Option<(u32, &str)> = None;
    for pid in &descendants {
        let comm = match read_comm(*pid) {
            Some(c) => c,
            None => continue,
        };
        for &(pattern, display_name) in AI_TOOLS {
            if comm.contains(pattern) {
                ai_pid = Some((*pid, display_name));
                break;
            }
        }
        if ai_pid.is_some() {
            break;
        }
    }

    let (tool_pid, tool_name) = match ai_pid {
        Some(v) => v,
        None => {
            // No AI tool found — check if one was previously running
            if let Some(prev) = prev_status {
                match prev {
                    AiStatus::Running { tool, .. } | AiStatus::Idle { tool, .. } => {
                        return (Some(AiStatus::Finished { tool: tool.clone() }), 0);
                    }
                    AiStatus::Finished { .. } => return (Some(prev.clone()), 0),
                }
            }
            return (None, 0);
        }
    };

    // Sum CPU time across the AI tool and all its descendants
    let ai_descendants = get_descendant_pids(tool_pid);
    let mut total_cpu: u64 = read_cpu_time(tool_pid).unwrap_or(0);
    for pid in &ai_descendants {
        total_cpu += read_cpu_time(*pid).unwrap_or(0);
    }

    let status = if total_cpu > prev_cpu_time && prev_cpu_time > 0 {
        AiStatus::Running {
            tool: tool_name.to_string(),
            pid: tool_pid,
        }
    } else {
        AiStatus::Idle {
            tool: tool_name.to_string(),
            pid: tool_pid,
        }
    };

    (Some(status), total_cpu)
}

/// Read utime + stime from /proc/{pid}/stat
fn read_cpu_time(pid: u32) -> Option<u64> {
    let content = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let after_comm = content.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // fields[0]=state, [1]=ppid, ..., [11]=utime, [12]=stime (0-indexed after comm)
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Get all descendant PIDs of a process by walking /proc
fn get_descendant_pids(root_pid: u32) -> Vec<u32> {
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

/// Read PPID from /proc/{pid}/stat
fn read_ppid(stat_path: &str) -> Option<u32> {
    let content = fs::read_to_string(stat_path).ok()?;
    let after_comm = content.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    fields.get(1)?.parse().ok()
}
