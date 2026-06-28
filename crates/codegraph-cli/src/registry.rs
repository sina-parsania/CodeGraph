//! Daemon-less TTL garbage collection. CodeGraph has no background process, so
//! an abandoned project's `.codegraph/` graph would otherwise sit on disk
//! forever. We keep a tiny registry of indexed projects with a last-use stamp
//! (`~/.config/codegraph/registry.json`) and, opportunistically on each run
//! (at most hourly), delete the graphs of projects untouched within the TTL.
//! "Use" = index OR query, so an actively-used project is never reclaimed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const DEFAULT_TTL_DAYS: u64 = 30;
const SECS_PER_DAY: u64 = 86_400;
/// Don't run the sweep more than once an hour from the opportunistic path.
const GC_MIN_INTERVAL_SECS: u64 = 3_600;

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    projects: BTreeMap<String, Entry>,
    #[serde(default)]
    last_gc: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    db: String,
    last_touch: u64,
    #[serde(default)]
    indexed_at: u64,
}

pub struct GcReport {
    pub removed: Vec<(String, u64)>,
    pub freed_bytes: u64,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn registry_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("codegraph").join("registry.json"))
}

fn load() -> Registry {
    registry_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(reg: &Registry) {
    let Some(p) = registry_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(reg) {
        let _ = std::fs::write(p, s);
    }
}

fn key(root: &Path) -> String {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf()).to_string_lossy().into_owned()
}

/// TTL in seconds from `CODEGRAPH_TTL_DAYS` (default 30; `0` disables auto-GC).
pub fn ttl_secs() -> u64 {
    std::env::var("CODEGRAPH_TTL_DAYS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TTL_DAYS)
        .saturating_mul(SECS_PER_DAY)
}

fn is_owned_graph_dir(d: &Path) -> bool {
    d.starts_with(crate::index::cache_root()) || d.file_name().is_some_and(|n| n == ".codegraph")
}

/// A registered project for `codegraph projects`: (root, db dir size, idle secs, exists).
pub struct ProjectInfo {
    pub root: String,
    pub bytes: u64,
    pub idle_secs: u64,
    pub exists: bool,
}

pub fn list_projects() -> Vec<ProjectInfo> {
    let reg = load();
    let t = now();
    let mut out: Vec<ProjectInfo> = reg
        .projects
        .into_iter()
        .map(|(root, e)| {
            let db = PathBuf::from(&e.db);
            let exists = db.exists();
            let bytes = db.parent().filter(|_| exists).map(dir_size).unwrap_or(0);
            ProjectInfo { root, bytes, idle_secs: t.saturating_sub(e.last_touch), exists }
        })
        .collect();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    out
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => total += dir_size(&e.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// Remove registry entries whose graph is gone, or (when `force_all` or idle >
/// `ttl_secs`) delete the project's `.codegraph/` dir. `ttl_secs == 0` without
/// `force_all` only prunes dangling entries (time-based deletion disabled).
fn sweep(reg: &mut Registry, ttl_secs: u64, force_all: bool, dry_run: bool) -> GcReport {
    let t = now();
    let mut report = GcReport { removed: Vec::new(), freed_bytes: 0 };
    let mut keep = BTreeMap::new();
    for (root, e) in std::mem::take(&mut reg.projects) {
        let db = PathBuf::from(&e.db);
        let cg_dir = db.parent().map(Path::to_path_buf);
        if !db.exists() {
            continue; // dangling entry — drop it
        }
        let idle = t.saturating_sub(e.last_touch);
        let expired = force_all || (ttl_secs > 0 && idle > ttl_secs);
        // Only ever delete a graph dir we own: one under the central cache, or a
        // legacy in-repo dir literally named ".codegraph".
        let safe_dir = cg_dir.filter(|d| is_owned_graph_dir(d));
        if let (true, Some(dir)) = (expired, safe_dir) {
            let bytes = dir_size(&dir);
            if !dry_run {
                let _ = std::fs::remove_dir_all(&dir);
            }
            report.freed_bytes += bytes;
            report.removed.push((root, bytes));
            continue;
        }
        keep.insert(root, e);
    }
    reg.projects = keep;
    report
}

/// Explicit `codegraph gc`: sweep with an idle threshold (or `force_all`).
pub fn run_gc(ttl_secs_override: Option<u64>, force_all: bool, dry_run: bool) -> GcReport {
    let mut reg = load();
    let ttl = ttl_secs_override.unwrap_or_else(|| {
        let env = ttl_secs();
        if env == 0 { DEFAULT_TTL_DAYS * SECS_PER_DAY } else { env }
    });
    let report = sweep(&mut reg, ttl, force_all, dry_run);
    if !dry_run {
        reg.last_gc = now();
        save(&reg);
    }
    report
}

/// Startup housekeeping: stamp the current project as used, and (at most hourly)
/// sweep expired graphs. Best-effort — never fails a command.
pub fn housekeeping(current: Option<(&Path, &Path, bool)>) {
    let mut reg = load();
    let t = now();
    if let Some((root, db, indexed)) = current {
        let e = reg.projects.entry(key(root)).or_insert(Entry {
            db: db.to_string_lossy().into_owned(),
            last_touch: t,
            indexed_at: 0,
        });
        e.db = db.to_string_lossy().into_owned();
        e.last_touch = t;
        if indexed {
            e.indexed_at = t;
        }
    }
    let ttl = ttl_secs();
    if ttl > 0 && t.saturating_sub(reg.last_gc) > GC_MIN_INTERVAL_SECS {
        sweep(&mut reg, ttl, false, false);
        reg.last_gc = t;
    }
    save(&reg);
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_removes_expired_keeps_fresh_and_prunes_dangling() {
        let tmp = std::env::temp_dir().join(format!("cg_gc_{}", std::process::id()));
        let old_db = tmp.join("old/.codegraph/graph.db");
        let new_db = tmp.join("new/.codegraph/graph.db");
        std::fs::create_dir_all(old_db.parent().unwrap()).unwrap();
        std::fs::create_dir_all(new_db.parent().unwrap()).unwrap();
        std::fs::write(&old_db, b"x").unwrap();
        std::fs::write(&new_db, b"y").unwrap();

        let t = now();
        let mut reg = Registry::default();
        let mk = |db: &Path, age: u64| Entry {
            db: db.to_string_lossy().into_owned(),
            last_touch: t.saturating_sub(age),
            indexed_at: 0,
        };
        reg.projects.insert("old".into(), mk(&old_db, 100 * SECS_PER_DAY));
        reg.projects.insert("new".into(), mk(&new_db, 0));
        reg.projects.insert("gone".into(), mk(&tmp.join("gone/.codegraph/graph.db"), 0));

        let report = sweep(&mut reg, 30 * SECS_PER_DAY, false, false);

        assert_eq!(report.removed.len(), 1, "only the expired graph is removed");
        assert!(!old_db.exists(), "expired .codegraph deleted");
        assert!(new_db.exists(), "fresh graph kept");
        assert!(reg.projects.contains_key("new"));
        assert!(!reg.projects.contains_key("old"));
        assert!(!reg.projects.contains_key("gone"), "dangling entry pruned");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
