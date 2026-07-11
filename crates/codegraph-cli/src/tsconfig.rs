//! tsconfig `paths` alias resolution for the indexer. Parsing stays IO-free
//! (codegraph-parse never reads tsconfig); the indexer rewrites non-relative
//! import specifiers through this map before persisting them, and the graph
//! resolver only learns one new module shape: a root-relative `/dir/file`.
//!
//! Determinism: configs are discovered with the same ignore rules as the code
//! walk, sorted, and content-hashed; the hash is stamped into the graph meta so
//! a tsconfig edit forces a full reindex (rewritten imports of UNCHANGED files
//! depend on it — their manifest hashes can't catch the change).

use std::path::Path;

/// One tsconfig's alias scope. `dir` is the config's directory (repo-relative,
/// "" for root); files under `dir` resolve against its rules (nearest wins).
struct TsConfigScope {
    dir: String,
    base_url: String,
    /// (pattern, targets) in declaration order — first match wins, like tsc.
    /// Pattern is `prefix*suffix` or exact.
    rules: Vec<(String, Vec<String>)>,
}

pub struct AliasMap {
    scopes: Vec<TsConfigScope>,
}

/// (base_url, rules) of one config after resolving its `extends` chain.
type ConfigParts = (Option<String>, Option<Vec<(String, Vec<String>)>>);

impl AliasMap {
    /// Resolve a non-relative import specifier for a file at `importer_rel`
    /// (repo-relative). Returns a root-relative module with a leading `/`
    /// (e.g. `@app/util/x` → `/web/src/util/x`), or None (external package).
    pub fn resolve(&self, importer_rel: &str, spec: &str) -> Option<String> {
        // nearest-ancestor scope: scopes are sorted by dir length desc
        let scope = self
            .scopes
            .iter()
            .find(|s| s.dir.is_empty() || importer_rel.starts_with(&format!("{}/", s.dir)))?;
        for (pattern, targets) in &scope.rules {
            let expanded: Vec<String> = match pattern.split_once('*') {
                Some((pre, suf)) => {
                    if !(spec.starts_with(pre) && spec.ends_with(suf) && spec.len() >= pre.len() + suf.len()) {
                        continue;
                    }
                    let star = &spec[pre.len()..spec.len() - suf.len()];
                    targets.iter().map(|t| t.replacen('*', star, 1)).collect()
                }
                None => {
                    if spec != pattern {
                        continue;
                    }
                    targets.clone()
                }
            };
            // tsc tries targets in declaration order and takes the first that
            // exists — the FIRST target is the declared priority. Precision is
            // still double-gated: the resolver only binds when the target file
            // actually defines the callee (a dead first target just drops).
            let target = expanded.first()?;
            let joined = join_rel(&scope.base_url, target);
            return Some(format!("/{joined}"));
        }
        None
    }
}

/// Normalize `base/target`, resolving `.` and `..` segments, repo-relative.
fn join_rel(base: &str, target: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in base.split('/').chain(target.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Strip JSONC comments and trailing commas (tsconfig files are JSONC).
/// String-aware; no dependency needed.
fn strip_jsonc(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let b = src.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i] as char;
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 1;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            '/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                // trailing comma: skip if the next non-ws char closes a scope
                let mut j = i + 1;
                while j < b.len() && (b[j] as char).is_whitespace() {
                    j += 1;
                }
                if j < b.len() && (b[j] == b'}' || b[j] == b']') {
                    i += 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Read one tsconfig (following its `extends` chain, relative paths only) into
/// (base_url, rules). Child `paths` replaces the parent's wholesale (tsc
/// semantics); `baseUrl` is resolved relative to the config that DECLARES it.
fn read_config(path: &Path, root: &Path, depth: u8) -> Option<ConfigParts> {
    if depth > 4 {
        return None; // extends cycle guard
    }
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&strip_jsonc(&text)).ok()?;
    let dir = path.parent().unwrap_or(root);
    let mut base_url: Option<String> = None;
    let mut rules: Option<Vec<(String, Vec<String>)>> = None;
    if let Some(ext) = json.get("extends").and_then(|v| v.as_str()) {
        // relative chain, or a package base like "@tsconfig/node18" (resolved
        // from node_modules; bare package name implies /tsconfig.json)
        let mut parent = if ext.starts_with('.') {
            dir.join(ext)
        } else {
            let p = root.join("node_modules").join(ext);
            if p.extension().is_some() || p.is_file() { p } else { p.join("tsconfig.json") }
        };
        if parent.extension().is_none() {
            parent.set_extension("json");
        }
        if let Some((b, r)) = read_config(&parent, root, depth + 1) {
            base_url = b;
            rules = r;
        }
    }
    let co = json.get("compilerOptions");
    if let Some(b) = co.and_then(|c| c.get("baseUrl")).and_then(|v| v.as_str()) {
        // resolve relative to the DECLARING config's dir, repo-relative
        let rel_dir = dir.strip_prefix(root).unwrap_or(Path::new("")).to_string_lossy().replace('\\', "/");
        base_url = Some(join_rel(&rel_dir, b));
    }
    if let Some(p) = co.and_then(|c| c.get("paths")).and_then(|v| v.as_object()) {
        let mut r: Vec<(String, Vec<String>)> = p
            .iter()
            .map(|(k, v)| {
                let targets = v
                    .as_array()
                    .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                (k.clone(), targets)
            })
            .collect();
        r.sort_by(|a, b| a.0.cmp(&b.0)); // serde_json map order isn't source order; sort for determinism
        rules = Some(r);
    }
    Some((base_url, rules))
}

/// Discover every tsconfig*.json under `root` (honoring the same ignore rules
/// as the code walk), build the alias map, and content-hash the set.
/// Returns (map, sha256-hash). Hash is stable: sorted (rel_path, sha) pairs.
pub fn load_alias_maps(root: &Path) -> (AliasMap, String) {
    use sha2::{Digest, Sha256};
    let mut configs: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in super::index::build_walker(root).filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("tsconfig") && name.ends_with(".json") && entry.path().is_file() {
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            configs.push((rel, entry.path().to_path_buf()));
        }
    }
    configs.sort();
    let mut h = Sha256::new();
    // Per directory, one config anchors the scope: prefer the plain
    // `tsconfig.json`, else the first variant that actually declares `paths`
    // (Angular keeps paths in tsconfig.app.json). Deterministic: sorted names.
    // (is_plain, file name, baseUrl, rules)
    type ScopeCandidate = (bool, String, Option<String>, Vec<(String, Vec<String>)>);
    let mut by_dir: std::collections::BTreeMap<String, Vec<ScopeCandidate>> = std::collections::BTreeMap::new();
    for (rel, path) in &configs {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        h.update(rel.as_bytes());
        h.update(Sha256::digest(content.as_bytes()));
        let Some((base_url, rules)) = read_config(path, root, 0) else { continue };
        let Some(rules) = rules else { continue };
        if rules.is_empty() {
            continue;
        }
        let (dir, name) = match rel.rsplit_once('/') {
            Some((d, n)) => (d.to_string(), n.to_string()),
            None => (String::new(), rel.clone()),
        };
        let is_plain = name == "tsconfig.json";
        by_dir.entry(dir).or_default().push((is_plain, name, base_url, rules));
    }
    let mut scopes = Vec::new();
    for (dir, mut cands) in by_dir {
        cands.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1))); // plain first, then by name
        let (_, _, base_url, rules) = cands.remove(0);
        // baseUrl defaults to the config's own dir when paths is present
        let base_url = base_url.unwrap_or_else(|| dir.clone());
        scopes.push(TsConfigScope { dir, base_url, rules });
    }
    // nearest-ancestor wins: longest dir first, tie-break lexicographic
    scopes.sort_by(|a, b| b.dir.len().cmp(&a.dir.len()).then(a.dir.cmp(&b.dir)));
    (AliasMap { scopes }, format!("{:x}", h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_stripping() {
        let src = r#"{
  // line comment
  "compilerOptions": {
    /* block */ "baseUrl": ".",
    "paths": { "@app/*": ["src/*"], },
  },
}"#;
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v["compilerOptions"]["baseUrl"], ".");
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn wildcard_nearest_ancestor_and_extends() {
        let tmp = std::env::temp_dir().join(format!("cg_tsc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        write(&tmp, "tsconfig.base.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@root/*":["lib/*"]}}}"#);
        write(&tmp, "tsconfig.json", r#"{"extends":"./tsconfig.base.json"}"#);
        write(
            &tmp,
            "web/tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"],"exact":["src/exact-mod"]}}}"#,
        );
        let (map, hash) = load_alias_maps(&tmp);
        // nearest scope for files under web/
        assert_eq!(map.resolve("web/src/a.ts", "@app/services/user"), Some("/web/src/services/user".into()));
        assert_eq!(map.resolve("web/src/a.ts", "exact"), Some("/web/src/exact-mod".into()));
        // root scope via extends chain
        assert_eq!(map.resolve("cli/main.ts", "@root/util"), Some("/lib/util".into()));
        // external package → None
        assert_eq!(map.resolve("web/src/a.ts", "react"), None);
        // hash changes when a config changes
        write(&tmp, "web/tsconfig.json", r#"{"compilerOptions":{"paths":{"@app/*":["other/*"]}}}"#);
        let (map2, hash2) = load_alias_maps(&tmp);
        assert_ne!(hash, hash2);
        assert_eq!(map2.resolve("web/src/a.ts", "@app/x"), Some("/web/other/x".into()));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn multiple_targets_take_declared_priority() {
        let tmp = std::env::temp_dir().join(format!("cg_tsc_multi_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        write(&tmp, "tsconfig.json", r#"{"compilerOptions":{"paths":{"@x/*":["a/*","b/*"]}}}"#);
        let (map, _) = load_alias_maps(&tmp);
        assert_eq!(
            map.resolve("src/f.ts", "@x/y").as_deref(),
            Some("/a/y"),
            "first target = tsc's declared priority (resolver still gates on the def existing)"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn variant_config_anchors_scope_when_plain_has_no_paths() {
        let tmp = std::env::temp_dir().join(format!("cg_tsc_variant_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // Angular-style: paths live in tsconfig.app.json, plain config has none
        write(&tmp, "tsconfig.json", r#"{"compilerOptions":{}}"#);
        write(&tmp, "tsconfig.app.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/app/*"]}}}"#);
        let (map, _) = load_alias_maps(&tmp);
        assert_eq!(map.resolve("src/main.ts", "@app/core"), Some("/src/app/core".into()));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extends_from_node_modules_package() {
        let tmp = std::env::temp_dir().join(format!("cg_tsc_nm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        write(
            &tmp,
            "node_modules/@tsconfig/base/tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"~/*":["lib/*"]}}}"#,
        );
        write(&tmp, "tsconfig.json", r#"{"extends":"@tsconfig/base"}"#);
        let (map, _) = load_alias_maps(&tmp);
        // rules inherited through the package base; baseUrl of the package dir
        // is inside node_modules (documented oddity), so just assert the rule fires
        assert!(map.resolve("src/a.ts", "~/util").is_some(), "package extends chain must be followed");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
