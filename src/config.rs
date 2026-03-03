use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct Preset {
    #[serde(rename = "project", default)]
    pub projects: Vec<ProjectPreset>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectPreset {
    pub name: String,
    pub path: String,
    #[serde(rename = "group", default)]
    pub groups: Vec<GroupPreset>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GroupPreset {
    pub name: String,
    pub path: Option<String>,
    pub worktree_branch: Option<String>,
    #[serde(rename = "window", default)]
    pub windows: Vec<WindowPreset>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WindowPreset {
    pub name: String,
    pub command: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub shell: Option<String>,
}

pub fn load_config() -> Config {
    let path = dirs_or_default().join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
            eprintln!("Warning: failed to parse {}: {}", path.display(), e);
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

fn presets_dir() -> PathBuf {
    let config = dirs_or_default();
    config.join("presets")
}

fn dirs_or_default() -> PathBuf {
    if let Some(config) = dirs::config_dir() {
        config.join("zmux")
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
            .join(".config")
            .join("zmux")
    }
}

pub fn load_preset(name: &str) -> Result<Preset> {
    let path = presets_dir().join(format!("{}.toml", name));
    load_preset_from_path(&path)
}

pub fn load_preset_from_path(path: &Path) -> Result<Preset> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read preset: {}", path.display()))?;
    let preset: Preset =
        toml::from_str(&content).with_context(|| format!("Failed to parse preset: {}", path.display()))?;
    Ok(preset)
}

pub fn list_presets() -> Result<Vec<String>> {
    let dir = presets_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut presets = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") {
            if let Some(stem) = path.file_stem() {
                presets.push(stem.to_string_lossy().to_string());
            }
        }
    }
    presets.sort();
    Ok(presets)
}

pub fn save_preset(name: &str, preset: &Preset) -> Result<()> {
    let dir = presets_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.toml", name));
    let content = toml::to_string_pretty(preset)
        .with_context(|| "Failed to serialize preset")?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write preset: {}", path.display()))?;
    Ok(())
}

/// Parse a .env file from the given directory. Returns empty map if no .env exists.
pub fn parse_dotenv(dir: &Path) -> HashMap<String, String> {
    let path = dir.join(".env");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let mut env = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim().to_string();
            let val = val.trim();
            // Strip surrounding quotes
            let val = if (val.starts_with('"') && val.ends_with('"'))
                || (val.starts_with('\'') && val.ends_with('\''))
            {
                val[1..val.len() - 1].to_string()
            } else {
                val.to_string()
            };
            env.insert(key, val);
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_dotenv_basic() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "FOO=bar\nBAZ=qux\n").unwrap();
        let env = parse_dotenv(dir.path());
        assert_eq!(env.get("FOO").unwrap(), "bar");
        assert_eq!(env.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn parse_dotenv_strips_quotes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "A=\"hello\"\nB='world'\n").unwrap();
        let env = parse_dotenv(dir.path());
        assert_eq!(env.get("A").unwrap(), "hello");
        assert_eq!(env.get("B").unwrap(), "world");
    }

    #[test]
    fn parse_dotenv_skips_comments_and_blanks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "# comment\n\nKEY=val\n").unwrap();
        let env = parse_dotenv(dir.path());
        assert_eq!(env.len(), 1);
        assert_eq!(env.get("KEY").unwrap(), "val");
    }

    #[test]
    fn parse_dotenv_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let env = parse_dotenv(dir.path());
        assert!(env.is_empty());
    }

    #[test]
    fn parse_dotenv_trims_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "  KEY  =  value  \n").unwrap();
        let env = parse_dotenv(dir.path());
        assert_eq!(env.get("KEY").unwrap(), "value");
    }

    #[test]
    fn preset_roundtrip() {
        let preset = Preset {
            projects: vec![ProjectPreset {
                name: "myproj".into(),
                path: "/tmp".into(),
                groups: vec![GroupPreset {
                    name: "main".into(),
                    path: None,
                    worktree_branch: Some("feature".into()),
                    windows: vec![WindowPreset {
                        name: "editor".into(),
                        command: Some("vim".into()),
                    }],
                }],
            }],
        };
        let toml_str = toml::to_string_pretty(&preset).unwrap();
        let decoded: Preset = toml::from_str(&toml_str).unwrap();
        assert_eq!(decoded.projects.len(), 1);
        assert_eq!(decoded.projects[0].groups[0].worktree_branch.as_deref(), Some("feature"));
        assert_eq!(decoded.projects[0].groups[0].windows[0].command.as_deref(), Some("vim"));
    }

    #[test]
    fn config_default() {
        let config = Config::default();
        assert!(config.shell.is_none());
    }
}
