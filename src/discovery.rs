use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use sysinfo::System;

use crate::path_utils::strip_windows_verbatim_prefix;

const ENV_VARS: &[&str] = &["COMFYUI_PATH", "COMFYUI_ROOT", "COMFYUI_HOME"];
const CONTAINER_NAMES: &[&str] = &["comfy", "pinokio", "stability"];
const MAX_SCAN_DEPTH: usize = 4;
const MAX_SCANNED_DIRS_PER_CONTAINER: usize = 600;

/// A possible ComfyUI installation found by discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComfyCandidate {
    pub path: PathBuf,
    pub source: String,
    pub score: u32,
    pub valid: bool,
    pub models_dir: Option<PathBuf>,
}

/// The complete result of one bounded discovery pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryReport {
    pub selected: Option<ComfyCandidate>,
    pub candidates: Vec<ComfyCandidate>,
    pub scanned_at: String,
}

/// A ComfyUI root must contain both of its stable Python entry files.
/// The `models` directory is intentionally optional because ComfyUI can be
/// configured to keep models elsewhere.
pub fn validate_comfyui_root(path: &Path) -> bool {
    path.is_dir() && path.join("main.py").is_file() && path.join("folder_paths.py").is_file()
}

/// Find a local ComfyUI installation without recursively scanning whole disks.
///
/// Sources, in descending priority, are an explicit path, environment
/// variables, running process commands/current directories, conventional user
/// directories, and keyword-named containers immediately below drive roots.
pub fn discover_comfyui(manual: Option<&Path>) -> DiscoveryReport {
    let mut collector = CandidateCollector::default();

    if let Some(path) = manual {
        collector.add_seed(path, "manual", 1_000, 2);
    }

    for (offset, variable) in ENV_VARS.iter().enumerate() {
        if let Some(value) = env::var_os(variable).filter(|value| !value.is_empty()) {
            collector.add_seed(
                Path::new(&value),
                &format!("environment:{variable}"),
                850u32.saturating_sub(offset as u32),
                2,
            );
        }
    }

    add_process_candidates(&mut collector);
    add_common_candidates(&mut collector);
    add_drive_container_candidates(&mut collector);

    collector.finish()
}

#[derive(Default)]
struct CandidateCollector {
    by_path: HashMap<String, ComfyCandidate>,
}

impl CandidateCollector {
    fn add_seed(&mut self, path: &Path, source: &str, score: u32, search_depth: usize) {
        let path = normalize_path(path);
        self.add_candidate(path.clone(), source, score);

        if !validate_comfyui_root(&path) && path.is_dir() && search_depth > 0 {
            for found in find_valid_roots(&path, search_depth, 160) {
                self.add_candidate(found, source, score.saturating_sub(10));
            }
        }
    }

    fn add_candidate(&mut self, path: PathBuf, source: &str, score: u32) {
        let path = normalize_path(&path);
        let valid = validate_comfyui_root(&path);
        let models_dir = path.join("models");
        let models_dir = models_dir.is_dir().then_some(models_dir);
        let score = score + u32::from(valid) * 100 + u32::from(models_dir.is_some()) * 10;
        let key = path_key(&path);

        match self.by_path.get_mut(&key) {
            Some(existing) => {
                if !existing.source.split(',').any(|item| item == source) {
                    existing.source.push(',');
                    existing.source.push_str(source);
                }
                if score > existing.score {
                    existing.score = score;
                }
                existing.valid |= valid;
                if existing.models_dir.is_none() {
                    existing.models_dir = models_dir;
                }
            }
            None => {
                self.by_path.insert(
                    key,
                    ComfyCandidate {
                        path,
                        source: source.to_owned(),
                        score,
                        valid,
                        models_dir,
                    },
                );
            }
        }
    }

    fn finish(self) -> DiscoveryReport {
        let mut candidates: Vec<_> = self.by_path.into_values().collect();
        candidates.sort_by(|left, right| {
            right
                .valid
                .cmp(&left.valid)
                .then_with(|| right.score.cmp(&left.score))
                .then_with(|| path_key(&left.path).cmp(&path_key(&right.path)))
        });
        let selected = candidates.iter().find(|candidate| candidate.valid).cloned();

        DiscoveryReport {
            selected,
            candidates,
            scanned_at: Utc::now().to_rfc3339(),
        }
    }
}

fn add_process_candidates(collector: &mut CandidateCollector) {
    let system = System::new_all();
    for process in system.processes().values() {
        let process_name = process.name().to_string_lossy().to_lowercase();
        let command = process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let command_lower = command.to_lowercase();
        let is_bridge =
            process_name.contains("comfyui-mcp") || command_lower.contains("comfyui-mcp-bridge");
        let looks_like_comfyui = command_lower.contains("main.py")
            || (process_name.contains("comfyui") && !process_name.contains("mcp"));
        if is_bridge || !looks_like_comfyui {
            continue;
        }

        if let Some(cwd) = process.cwd() {
            collector.add_seed(cwd, "running-process:cwd", 750, 2);
        }
        for part in process.cmd() {
            let path = PathBuf::from(part);
            if path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("main.py"))
            {
                if let Some(parent) = path.parent() {
                    collector.add_seed(parent, "running-process:command", 780, 1);
                }
            } else if part.to_string_lossy().to_lowercase().contains("comfy") {
                collector.add_seed(&path, "running-process:command", 770, 1);
            }
        }
    }
}

fn add_common_candidates(collector: &mut CandidateCollector) {
    if let Ok(current) = env::current_dir() {
        collector.add_seed(&current, "common:current-directory", 520, 2);
    }
    if let Ok(executable) = env::current_exe()
        && let Some(parent) = executable.parent()
    {
        collector.add_seed(parent, "common:executable-directory", 510, 2);
    }

    if let Some(profile) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
        let profile = PathBuf::from(profile);
        for relative in [
            "ComfyUI",
            "Desktop/ComfyUI",
            "Documents/ComfyUI",
            "Downloads/ComfyUI",
            "pinokio/api/comfyui.git/app",
            "AppData/Roaming/StabilityMatrix/Packages/ComfyUI",
        ] {
            collector.add_seed(&profile.join(relative), "common:user-directory", 500, 2);
        }
    }
}

fn add_drive_container_candidates(collector: &mut CandidateCollector) {
    for root in drive_roots() {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || !contains_container_keyword(&entry.file_name().to_string_lossy()) {
                continue;
            }
            collector.add_candidate(path.clone(), "drive-keyword-container", 300);
            for found in find_valid_roots(&path, MAX_SCAN_DEPTH, MAX_SCANNED_DIRS_PER_CONTAINER) {
                collector.add_candidate(found, "drive-keyword-container", 390);
            }
        }
    }
}

#[cfg(windows)]
fn drive_roots() -> Vec<PathBuf> {
    (b'A'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
        .filter(|root| root.is_dir())
        .collect()
}

#[cfg(not(windows))]
fn drive_roots() -> Vec<PathBuf> {
    Vec::new()
}

fn find_valid_roots(start: &Path, max_depth: usize, max_dirs: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue = VecDeque::from([(start.to_path_buf(), 0usize)]);
    let mut visited = 0usize;

    while let Some((directory, depth)) = queue.pop_front() {
        if visited >= max_dirs {
            break;
        }
        visited += 1;

        if validate_comfyui_root(&directory) {
            found.push(directory);
            continue;
        }
        if depth >= max_depth {
            continue;
        }

        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
            {
                queue.push_back((path, depth + 1));
            }
        }
    }
    found
}

fn contains_container_keyword(name: &str) -> bool {
    let lower = name.to_lowercase();
    CONTAINER_NAMES
        .iter()
        .any(|keyword| lower.contains(keyword))
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .map(strip_windows_verbatim_prefix)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_comfy_root(path: &Path, with_models: bool) {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("main.py"), "# test").unwrap();
        fs::write(path.join("folder_paths.py"), "# test").unwrap();
        if with_models {
            fs::create_dir(path.join("models")).unwrap();
        }
    }

    #[test]
    fn validation_requires_both_python_files_but_not_models() {
        let temp = tempdir().unwrap();
        assert!(!validate_comfyui_root(temp.path()));
        fs::write(temp.path().join("main.py"), "").unwrap();
        assert!(!validate_comfyui_root(temp.path()));
        fs::write(temp.path().join("folder_paths.py"), "").unwrap();
        assert!(validate_comfyui_root(temp.path()));
    }

    #[test]
    fn nested_search_is_bounded_by_depth() {
        let temp = tempdir().unwrap();
        let shallow = temp.path().join("one/two");
        let deep = temp.path().join("one/two/three/four");
        make_comfy_root(&shallow, false);
        make_comfy_root(&deep, false);

        assert_eq!(find_valid_roots(temp.path(), 1, 100), Vec::<PathBuf>::new());
        assert_eq!(find_valid_roots(temp.path(), 2, 100), vec![shallow]);
    }

    #[test]
    fn collector_deduplicates_and_prefers_manual_candidate() {
        let temp = tempdir().unwrap();
        let manual = temp.path().join("ComfyUI");
        let other = temp.path().join("OtherComfyUI");
        make_comfy_root(&manual, true);
        make_comfy_root(&other, false);

        let mut collector = CandidateCollector::default();
        collector.add_seed(&other, "common", 500, 0);
        collector.add_seed(&manual, "manual", 1_000, 0);
        collector.add_seed(&manual, "environment:COMFYUI_PATH", 850, 0);
        let report = collector.finish();

        assert_eq!(report.candidates.len(), 2);
        let selected = report.selected.unwrap();
        assert_eq!(
            selected.path,
            strip_windows_verbatim_prefix(fs::canonicalize(manual).unwrap())
        );
        assert!(selected.models_dir.is_some());
        assert!(selected.source.contains("manual"));
        assert!(selected.source.contains("environment:COMFYUI_PATH"));
    }

    #[test]
    fn invalid_candidates_are_reported_but_not_selected() {
        let temp = tempdir().unwrap();
        let mut collector = CandidateCollector::default();
        collector.add_seed(temp.path(), "manual", 1_000, 0);
        let report = collector.finish();

        assert!(report.selected.is_none());
        assert_eq!(report.candidates.len(), 1);
        assert!(!report.candidates[0].valid);
    }
}
