//! Graph build layer: turn parsed nodes + raw calls into a petgraph graph and
//! the persisted edge set. M3 lands structural (DEFINES) + intra-file CALLS
//! resolution (Tier-B, same-language only — no cross-language call edges).

use std::collections::{HashMap, HashSet};

use codegraph_core::{
    Confidence, Edge, EdgeRelation, Hyperedge, HyperedgeMember, HyperedgeRelation, InheritKind, Metadata,
    Node, NodeLabel, RawCall, RawField, RawImport, RawInherit, RawLocal, Receiver, ResolutionTier,
};

/// Class-Hierarchy-Analysis member resolution (docs/RESOLUTION.md): find the method
/// `name` on `class_id` or its nearest ancestor; return Some iff exactly one such
/// method exists at the nearest level, else None (DROP — never guess).
fn resolve_member<'a>(
    class_id: &str,
    name: &str,
    class_members: &HashMap<&'a str, HashMap<&'a str, Vec<&'a str>>>,
    class_parents: &HashMap<&'a str, Vec<&'a str>>,
) -> Option<&'a str> {
    if let Some(ms) = class_members.get(class_id).and_then(|m| m.get(name)) {
        return (ms.len() == 1).then(|| ms[0]);
    }
    let mut frontier: Vec<&str> = class_parents.get(class_id).cloned().unwrap_or_default();
    let mut visited: HashSet<&str> = HashSet::new();
    while !frontier.is_empty() {
        let mut matches: Vec<&str> = Vec::new();
        let mut next: Vec<&str> = Vec::new();
        for p in &frontier {
            if !visited.insert(p) {
                continue;
            }
            if let Some(ms) = class_members.get(*p).and_then(|m| m.get(name)) {
                matches.extend(ms.iter().copied());
            }
            next.extend(class_parents.get(*p).cloned().unwrap_or_default());
        }
        if !matches.is_empty() {
            return (matches.len() == 1).then(|| matches[0]);
        }
        frontier = next;
    }
    None
}

/// Top-level path component = the monorepo project boundary for name-based
/// resolution. `web-app/app/x.ts` → `web-app`; a flat root file (`a.rs`, no
/// slash) → `""` so single-project / test layouts impose no scoping. Name-based
/// global resolution is only evidence WITHIN one such segment: a merged monorepo
/// otherwise makes a name "globally unique" across unrelated services and binds
/// phantom cross-project edges.
fn top_segment(path: &str) -> &str {
    match path.split_once('/') {
        Some((head, _)) => head,
        None => "",
    }
}

/// T6: bind a bare call to the ONE file its import points at. The import is the
/// evidence; still unique-or-drop (no def in the resolved file → None, never guess).
/// TS/JS: relative module → sibling file with known extensions (or /index.*).
/// Python: dotted module → path with .py (searched by suffix match).
fn resolve_via_import<'a>(
    caller: &Node,
    callee: &str,
    import_map: &HashMap<(&str, &str), Vec<&'a str>>,
    fn_by_file_name: &HashMap<(&'a str, &'a str), &'a str>,
    fn_by_name: &HashMap<&'a str, Vec<&'a str>>,
    by_id: &HashMap<&str, &'a Node>,
) -> Option<&'a str> {
    let modules = import_map.get(&(caller.file_path.as_str(), callee))?;
    if modules.len() != 1 {
        return None; // conflicting imports of the same name → drop
    }
    let module = modules[0];
    let dir = caller.file_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut hits: Vec<&str> = Vec::new();
    if module.starts_with('.') || module.starts_with('/') {
        // TS/JS relative: normalize ./ ../ against the caller's dir.
        // A leading `/` is a ROOT-relative module (a tsconfig-paths alias the
        // indexer already expanded) — same evidence class, same probing.
        let base = if let Some(root_rel) = module.strip_prefix('/') {
            join_normalize("", root_rel)
        } else {
            join_normalize(dir, module)
        };
        for cand in [
            format!("{base}.ts"), format!("{base}.tsx"), format!("{base}.js"),
            format!("{base}.jsx"), format!("{base}.mjs"),
            format!("{base}/index.ts"), format!("{base}/index.tsx"), format!("{base}/index.js"),
        ] {
            if let Some(&id) = fn_by_file_name.get(&(cand.as_str(), callee)) {
                if !hits.contains(&id) {
                    hits.push(id);
                }
            }
        }
    } else if caller.language == "python" {
        // Python dotted: a.b.c → …/a/b/c.py — match by path suffix (repo-root
        // unknown). Iterate the few same-name candidates, not every function in
        // the repo (this runs once per unresolved import-bound call site).
        let root = module.replace('.', "/");
        let tail = format!("{root}.py");
        let init = format!("{root}/__init__.py");
        for &id in fn_by_name.get(callee).map(Vec::as_slice).unwrap_or_default() {
            let Some(n) = by_id.get(id) else { continue };
            let file = n.file_path.as_str();
            if (file.ends_with(&tail) || file.ends_with(&init)) && !hits.contains(&id) {
                hits.push(id);
            }
        }
    }
    (hits.len() == 1).then(|| hits[0])
}

/// Join `module` (./x, ../y/z) onto `dir`, resolving . and .. segments.
fn join_normalize(dir: &str, module: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in module.split('/') {
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


/// Test-file predicate shared with the store's SQL queries (token-aware,
/// `latest_prices.rs` is not a test) — one definition in codegraph-core so the
/// TESTS edges and the dead-code/test-coverage queries can never disagree.
use codegraph_core::is_test_path;

/// Entry points for flow detection: HTTP route handlers (resolved by name within
/// the route's file), `main`, and zero-fan-in functions that call 3+ others
/// (likely tasks/jobs). Each tagged with its kind.
pub fn detect_entry_points(nodes: &[Node]) -> Vec<(&Node, &'static str)> {
    use NodeLabel::*;
    let mut out: Vec<(&Node, &'static str)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut fn_by_file_name: HashMap<(&str, &str), &Node> = HashMap::new();
    for n in nodes.iter().filter(|n| matches!(n.label, Function | Method)) {
        // first definition wins — matches the previous linear-scan semantics
        fn_by_file_name.entry((n.file_path.as_str(), n.name.as_str())).or_insert(n);
    }
    for r in nodes.iter().filter(|n| n.label == Route) {
        if let Some(h) = r.metadata.get("handler").and_then(|v| v.as_str()) {
            if let Some(&f) = fn_by_file_name.get(&(r.file_path.as_str(), h)) {
                if seen.insert(&f.id) {
                    out.push((f, "route"));
                }
            }
        }
    }
    for f in nodes.iter().filter(|n| matches!(n.label, Function | Method)) {
        let fan_in = f.metadata.get("fan_in").and_then(|v| v.as_u64()).unwrap_or(0);
        let fan_out = f.metadata.get("fan_out").and_then(|v| v.as_u64()).unwrap_or(0);
        let is_main = f.name == "main";
        if (is_main || (fan_in == 0 && fan_out >= 3)) && seen.insert(&f.id) {
            out.push((f, if is_main { "main" } else { "task" }));
        }
    }
    out
}


use petgraph::stable_graph::{NodeIndex, StableGraph};

/// Directed graph of node-id → node-id, edge weight = the relation itself
/// (a Copy enum — integer compares in traversals, no per-edge String alloc).
pub type CodeGraph = StableGraph<String, EdgeRelation>;

pub struct Built {
    pub graph: CodeGraph,
    pub edges: Vec<Edge>,
    pub hyperedges: Vec<(Hyperedge, Vec<HyperedgeMember>)>,
}

/// Build the edge set + petgraph from parsed nodes and unresolved calls.
/// - Pass 1 (structural): each File DEFINES every definition in the same file.
/// - Pass 2 (calls): resolve each `RawCall` to a Function in the caller's file
///   by name (intra-language, intra-file) → CALLS edge tagged Tier B.
pub fn build(
    nodes: &[Node],
    calls: &[RawCall],
    inherits: &[RawInherit],
    fields: &[RawField],
    locals: &[RawLocal],
    imports: &[RawImport],
) -> Built {
    build_with(nodes, calls, inherits, fields, locals, imports, false)
}

/// PARTIAL edge rebuild for incremental indexing. Resolution tables are built
/// from the FULL current inputs (a call's resolution depends on global name
/// uniqueness, the class hierarchy, and field types — all repo-wide), but only
/// `calls_in_changed` are resolved, and the result is filtered to edges
/// ORIGINATING in `changed` files — the slice the indexer deletes and replaces.
///
/// Sound ONLY when every changed file's interface signature (node ids/labels,
/// inherits, fields) is unchanged — the indexer checks this. Then no OTHER
/// file's resolution environment changed, so its stored edges remain exactly
/// what a full rebuild would produce, and hyperedges/implementer sets are
/// untouched. Same code path as `build_with` ⇒ byte-identical edges.
#[allow(clippy::too_many_arguments)]
pub fn resolve_files(
    nodes: &[Node],
    calls_in_changed: &[RawCall],
    inherits: &[RawInherit],
    fields: &[RawField],
    locals: &[RawLocal],
    imports: &[RawImport],
    include_ambiguous: bool,
    changed: &HashSet<String>,
) -> Vec<Edge> {
    let built = build_with(nodes, calls_in_changed, inherits, fields, locals, imports, include_ambiguous);
    built.edges.into_iter().filter(|e| changed.contains(&e.src_file)).collect()
}

/// `include_ambiguous`: ALSO emit edges for unresolved calls with 2–5 same-name
/// candidates, tagged `Confidence::Ambiguous` + justification "Ambiguous" — an
/// honest opt-in recall tier (coverage/queries can filter them; never silent).
pub fn build_with(
    nodes: &[Node],
    calls: &[RawCall],
    inherits: &[RawInherit],
    fields: &[RawField],
    locals: &[RawLocal],
    imports: &[RawImport],
    include_ambiguous: bool,
) -> Built {
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let file_by_path: HashMap<&str, &str> = nodes
        .iter()
        .filter(|n| n.label == NodeLabel::File)
        .map(|n| (n.file_path.as_str(), n.id.as_str()))
        .collect();
    let mut fn_by_file_name: HashMap<(&str, &str), &str> = HashMap::new();
    let mut fn_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for n in nodes.iter().filter(|n| matches!(n.label, NodeLabel::Function | NodeLabel::Method)) {
        fn_by_file_name.insert((n.file_path.as_str(), n.name.as_str()), n.id.as_str());
        fn_by_name.entry(n.name.as_str()).or_default().push(n.id.as_str());
    }

    // Class-Hierarchy-Analysis tables for receiver-aware (self/this) resolution.
    let class_nodes: Vec<&Node> = nodes
        .iter()
        .filter(|n| matches!(n.label, NodeLabel::Class | NodeLabel::Interface | NodeLabel::Enum))
        .collect();
    let mut class_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for c in &class_nodes {
        class_by_name.entry(c.name.as_str()).or_default().push(c.id.as_str());
    }
    // class_members: class id -> method name -> method ids, by innermost containment.
    // Includes Function nodes too (Swift/Python/Rust/Kotlin label methods Function).
    // Classes are pre-grouped by file so each method only scans its own file's
    // classes — the all-classes scan was O(methods × classes) repo-wide.
    let mut classes_by_file: HashMap<&str, Vec<&Node>> = HashMap::new();
    for c in &class_nodes {
        classes_by_file.entry(c.file_path.as_str()).or_default().push(c);
    }
    let mut class_members: HashMap<&str, HashMap<&str, Vec<&str>>> = HashMap::new();
    for m in nodes.iter().filter(|n| matches!(n.label, NodeLabel::Method | NodeLabel::Function)) {
        let owner = classes_by_file
            .get(m.file_path.as_str())
            .into_iter()
            .flatten()
            .filter(|c| c.line_start <= m.line_start && m.line_end <= c.line_end && c.id != m.id)
            .min_by_key(|c| c.line_end - c.line_start);
        if let Some(c) = owner {
            class_members.entry(c.id.as_str()).or_default().entry(m.name.as_str()).or_default().push(m.id.as_str());
        }
    }
    // class_parents: child class id -> parent class ids (resolved by unique name).
    let mut class_parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for inh in inherits {
        if let (Some(ch), Some(pa)) =
            (class_by_name.get(inh.impl_name.as_str()), class_by_name.get(inh.super_name.as_str()))
        {
            if ch.len() == 1 && pa.len() == 1 {
                class_parents.entry(ch[0]).or_default().push(pa[0]);
            }
        }
    }
    // field_types: class id -> field name -> declared type name (for T3 / DI).
    let mut field_types: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    for f in fields {
        field_types
            .entry(f.class_id.as_str())
            .or_default()
            .insert(f.field_name.as_str(), f.type_name.as_str());
    }
    // import_map: (file, bound name) -> imported-from modules (T6 evidence).
    let mut import_map: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for im in imports {
        import_map.entry((im.file_path.as_str(), im.name.as_str())).or_default().push(im.module.as_str());
    }
    // fn_by_dir_name: (dir, name) -> fn ids (Go package scope: dir == package).
    let mut fn_by_dir_name: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for n in nodes.iter().filter(|n| matches!(n.label, NodeLabel::Function | NodeLabel::Method)) {
        let dir = n.file_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        fn_by_dir_name.entry((dir, n.name.as_str())).or_default().push(n.id.as_str());
    }

    // local_types: caller fn id -> local var name -> inferred type name (T5).
    // An EMPTY type is a SHADOW marker (an untyped parameter/local binding):
    // it can't type a member call, but it proves the name is locally bound —
    // a typed entry for the same name always outranks it.
    let mut local_types: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    for l in locals {
        let m = local_types.entry(l.caller_id.as_str()).or_default();
        match m.get(l.var_name.as_str()) {
            Some(t) if !t.is_empty() => {}
            _ => {
                m.insert(l.var_name.as_str(), l.type_name.as_str());
            }
        }
    }

    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: HashSet<(String, String, EdgeRelation)> = HashSet::new();

    for n in nodes.iter().filter(|n| n.label != NodeLabel::File) {
        if let Some(&file_id) = file_by_path.get(n.file_path.as_str()) {
            push_edge(&mut edges, &mut seen, Edge {
                src: file_id.to_string(),
                dst: n.id.clone(),
                relation: EdgeRelation::Defines,
                tier: ResolutionTier::TreeSitter,
                confidence: Confidence::Extracted,
                src_file: n.file_path.clone(),
                src_line: n.line_start,
                metadata: Metadata::new(),
            });
        }
    }

    // Route-hub ids present in this graph (for exact-id HTTP linking).
    let route_ids: HashSet<&str> =
        nodes.iter().filter(|n| n.label == NodeLabel::Route).map(|n| n.id.as_str()).collect();

    for c in calls {
        let Some(caller) = by_id.get(c.caller_id.as_str()) else { continue };
        // HTTP hub link: callee is a route id (exact path+method key) -> direct edge.
        if c.callee_name.starts_with("route.") && route_ids.contains(c.callee_name.as_str()) {
            let mut metadata = Metadata::new();
            metadata.insert("justification".to_string(), serde_json::Value::String("HttpPath".to_string()));
            push_edge(&mut edges, &mut seen, Edge {
                src: c.caller_id.clone(),
                dst: c.callee_name.clone(),
                relation: EdgeRelation::HttpCalls,
                tier: ResolutionTier::TreeSitter,
                confidence: Confidence::Extracted,
                src_file: caller.file_path.clone(),
                src_line: c.line,
                metadata,
            });
            continue;
        }
        // Receiver-aware tiers (provably correct, unique-or-drop, never a guess):
        // T1 self/this -> enclosing class; T3 this.field.method() -> the field's
        // declared type's class. Everything else falls back to the existing
        // same-file / project-wide-unique path (no regression).
        // Each resolution carries a `justification` tag = the tier that resolved
        // it (the precision proof obligation + per-tier measurement surface).
        let receiver_resolved: Option<(&str, &'static str)> = match &c.receiver {
            Receiver::SelfThis => c
                .enclosing_class
                .as_deref()
                .and_then(|cls| resolve_member(cls, &c.callee_name, &class_members, &class_parents))
                .map(|id| (id, "SelfThisMember")),
            Receiver::Field(field) => c
                .enclosing_class
                .as_deref()
                .and_then(|cls| field_types.get(cls).and_then(|m| m.get(field.as_str())).copied())
                .and_then(|ty| match class_by_name.get(ty) {
                    Some(ids) if ids.len() == 1 => Some(ids[0]),
                    _ => None,
                })
                .and_then(|type_cls| resolve_member(type_cls, &c.callee_name, &class_members, &class_parents))
                .map(|id| (id, "FieldTypeMember")),
            // `name.method()`: resolve `name`'s static type, then that type's method.
            // A local/param SHADOWS a field (correct lexical scoping); failing that,
            // an implicit-`this` field of the enclosing class (Kotlin/Swift/Java write
            // `field.method()` without `this.`). Unique-or-drop throughout.
            Receiver::Named(var) => {
                let via_local = local_types
                    .get(c.caller_id.as_str())
                    .and_then(|m| m.get(var.as_str()))
                    .copied()
                    // a shadow marker (untyped param/local) is a binding without a
                    // type: it must BLOCK the field fallback (lexical scoping) yet
                    // resolve nothing — hence kept as Some with an empty type
                    .map(|ty| (ty, "LocalVarType"));
                let via_field = c
                    .enclosing_class
                    .as_deref()
                    .and_then(|cls| field_types.get(cls).and_then(|m| m.get(var.as_str())).copied())
                    .map(|ty| (ty, "FieldTypeMember"));
                via_local
                    .or(via_field)
                    .and_then(|(ty, tag)| match class_by_name.get(ty) {
                        Some(ids) if ids.len() == 1 => Some((ids[0], tag)),
                        _ => None,
                    })
                    .and_then(|(type_cls, tag)| {
                        resolve_member(type_cls, &c.callee_name, &class_members, &class_parents).map(|id| (id, tag))
                    })
            }
            _ => None,
        };
        let global_unique = || match fn_by_name.get(c.callee_name.as_str()) {
            Some(cands) if cands.len() == 1 => Some(cands[0]),
            _ => None,
        };
        let resolved: Option<(&str, &'static str)> = receiver_resolved.or_else(|| match &c.receiver {
            // Unqualified `foo()`: a LOCAL BINDING (param/var named foo) shadows
            // everything — calling through it must never bind a free function
            // (audit-caught: a `call_next` param bound to another file's def).
            // Otherwise: same-file first, then the file's imports (T6 — the
            // import IS the evidence), then Go package scope, then global-unique.
            Receiver::Bare => {
                if local_types
                    .get(c.caller_id.as_str())
                    .and_then(|m| m.get(c.callee_name.as_str()))
                    .is_some()
                {
                    return None;
                }
                fn_by_file_name
                    .get(&(caller.file_path.as_str(), c.callee_name.as_str()))
                    .copied()
                    .map(|id| (id, "SameFileUnique"))
                    .or_else(|| {
                        resolve_via_import(caller, &c.callee_name, &import_map, &fn_by_file_name, &fn_by_name, &by_id)
                            .map(|id| (id, "ImportNarrowed"))
                    })
                    .or_else(|| {
                        (caller.language == "go")
                            .then(|| {
                                let dir = caller.file_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                                match fn_by_dir_name.get(&(dir, c.callee_name.as_str())) {
                                    Some(ids) if ids.len() == 1 => Some(ids[0]),
                                    _ => None,
                                }
                            })
                            .flatten()
                            .map(|id| (id, "PackageScope"))
                    })
                    .or_else(|| global_unique().map(|id| (id, "GlobalUnique")))
            }
            // Qualified call we couldn't type (named var / super / dropped self or
            // field): the receiver's EXISTENCE is evidence the callee is a member
            // of some (unknown) type — binding it to a repo-unique free function
            // ignores that evidence. Audit measured this guess at 27% precision
            // on a real Python repo (was tagged GlobalUnique). Drop, never guess.
            _ => None,
        });
        if let Some((callee_id, justification)) = resolved {
            if callee_id == c.caller_id {
                continue;
            }
            // Name/type-inferred resolution is only evidence WITHIN one language and
            // one top-level project segment — a merged monorepo otherwise binds
            // phantom cross-service / cross-language edges via same-name or
            // same-type-name collisions. `ImportNarrowed` is exempt: the explicit
            // import IS the cross-boundary evidence (shared-package monorepos).
            if justification != "ImportNarrowed" {
                if let Some(cand) = by_id.get(callee_id) {
                    if cand.language != caller.language
                        || top_segment(&cand.file_path) != top_segment(&caller.file_path)
                    {
                        continue;
                    }
                }
            }
            let mut metadata = Metadata::new();
            metadata.insert("justification".to_string(), serde_json::Value::String(justification.to_string()));
            push_edge(&mut edges, &mut seen, Edge {
                src: c.caller_id.clone(),
                dst: callee_id.to_string(),
                relation: EdgeRelation::Calls,
                tier: ResolutionTier::TreeSitter,
                confidence: Confidence::Inferred,
                src_file: caller.file_path.clone(),
                src_line: c.line,
                metadata: metadata.clone(),
            });
            // First-class TESTS edge: a RESOLVED call from a test file covers the
            // callee (projection of Calls — same precision, no extra guessing).
            if is_test_path(&caller.file_path) {
                push_edge(&mut edges, &mut seen, Edge {
                    src: c.caller_id.clone(),
                    dst: callee_id.to_string(),
                    relation: EdgeRelation::Tests,
                    tier: ResolutionTier::TreeSitter,
                    confidence: Confidence::Inferred,
                    src_file: caller.file_path.clone(),
                    src_line: c.line,
                    metadata,
                });
            }
        } else if include_ambiguous {
            // Opt-in honest-recall tier: an unresolved call with a SMALL candidate
            // set (2–5 same-name defs) emits an edge to EACH, clearly tagged
            // Ambiguous — never mixed into the precise default graph.
            if let Some(cands) = fn_by_name.get(c.callee_name.as_str()) {
                if (2..=5).contains(&cands.len()) {
                    for &cand in cands.iter() {
                        if cand == c.caller_id {
                            continue;
                        }
                        let mut metadata = Metadata::new();
                        metadata.insert("justification".to_string(), serde_json::Value::String("Ambiguous".to_string()));
                        push_edge(&mut edges, &mut seen, Edge {
                            src: c.caller_id.clone(),
                            dst: cand.to_string(),
                            relation: EdgeRelation::Calls,
                            tier: ResolutionTier::TreeSitter,
                            confidence: Confidence::Ambiguous,
                            src_file: caller.file_path.clone(),
                            src_line: c.line,
                            metadata,
                        });
                    }
                }
            }
        }
    }

    // Inheritance edges (resolved by unique project-wide name) + IMPLEMENTS hyperedges.
    let mut node_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for n in nodes.iter().filter(|n| n.label != NodeLabel::File) {
        node_by_name.entry(n.name.as_str()).or_default().push(n.id.as_str());
    }
    let mut implementers: HashMap<&str, Vec<String>> = HashMap::new();
    for inh in inherits {
        let imp_id = match node_by_name.get(inh.impl_name.as_str()) {
            Some(v) if v.len() == 1 => v[0],
            _ => continue,
        };
        let sup_id = match node_by_name.get(inh.super_name.as_str()) {
            Some(v) if v.len() == 1 => v[0],
            _ => continue,
        };
        if imp_id == sup_id {
            continue;
        }
        // The resolved target's label outranks the parser's syntactic guess:
        // Swift `class A: B` can't tell a superclass from a protocol at parse
        // time, but the graph knows whether B is an Interface. Exception:
        // `interface I extends J` is interface REFINEMENT, not implementation —
        // implementers() must keep returning concrete types only.
        let imp_label = by_id.get(imp_id).map(|n| n.label);
        let relation = match (by_id.get(sup_id).map(|n| n.label), inh.kind) {
            (Some(NodeLabel::Interface), _) if imp_label == Some(NodeLabel::Interface) => EdgeRelation::Inherits,
            (Some(NodeLabel::Interface), _) => EdgeRelation::Implements,
            (Some(NodeLabel::Class), _) => EdgeRelation::Inherits,
            (_, InheritKind::Extends) => EdgeRelation::Inherits,
            (_, InheritKind::Implements) => EdgeRelation::Implements,
        };
        push_edge(&mut edges, &mut seen, Edge {
            src: imp_id.to_string(),
            dst: sup_id.to_string(),
            relation,
            tier: ResolutionTier::TreeSitter,
            confidence: Confidence::Extracted,
            src_file: by_id.get(imp_id).map(|n| n.file_path.clone()).unwrap_or_default(),
            src_line: by_id.get(imp_id).map(|n| n.line_start).unwrap_or(1),
            metadata: Metadata::new(),
        });
        if relation == EdgeRelation::Implements {
            implementers.entry(sup_id).or_default().push(imp_id.to_string());
        }
    }
    let mut hyperedges: Vec<(Hyperedge, Vec<HyperedgeMember>)> = Vec::new();
    let mut sup_ids: Vec<&str> = implementers.keys().copied().collect();
    sup_ids.sort();
    for sup_id in sup_ids {
        let impls = &implementers[sup_id];
        if impls.len() < 2 {
            continue;
        }
        let hid = format!("implements::{}", sup_id);
        let sup_name = by_id.get(sup_id).map(|n| n.name.as_str()).unwrap_or("");
        let h = Hyperedge {
            id: hid.clone(),
            relation: HyperedgeRelation::Implement,
            label: format!("implementers of {}", sup_name),
            confidence: Confidence::Extracted,
            tier: ResolutionTier::TreeSitter,
            metadata: Metadata::new(),
        };
        let mut members: Vec<HyperedgeMember> = impls
            .iter()
            .map(|id| HyperedgeMember { hyperedge_id: hid.clone(), node_id: id.clone(), role: Some("implementer".to_string()) })
            .collect();
        members.push(HyperedgeMember { hyperedge_id: hid.clone(), node_id: sup_id.to_string(), role: Some("interface".to_string()) });
        hyperedges.push((h, members));
    }

    let mut graph = CodeGraph::new();
    let mut idx: HashMap<&str, NodeIndex> = HashMap::new();
    for n in nodes {
        idx.insert(n.id.as_str(), graph.add_node(n.id.clone()));
    }
    for e in &edges {
        if let (Some(&a), Some(&b)) = (idx.get(e.src.as_str()), idx.get(e.dst.as_str())) {
            graph.add_edge(a, b, e.relation);
        }
    }

    Built { graph, edges, hyperedges }
}

fn push_edge(
    edges: &mut Vec<Edge>,
    seen: &mut HashSet<(String, String, EdgeRelation)>,
    e: Edge,
) {
    let key = (e.src.clone(), e.dst.clone(), e.relation);
    if seen.insert(key) {
        edges.push(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_store::Store;

    #[test]
    fn structural_and_call_edges() {
        let pf = codegraph_parse::parse_rust("proj", "src/main.rs", "fn helper() {}\nfn main() { helper(); helper(); }\n");
        let built = build(&pf.nodes, &pf.calls, &pf.inherits, &pf.fields, &pf.locals, &pf.imports);

        let calls: Vec<_> = built.edges.iter().filter(|e| e.relation == EdgeRelation::Calls).collect();
        assert_eq!(calls.len(), 1, "duplicate calls should dedupe to one edge");
        assert!(calls[0].src.ends_with("main") && calls[0].dst.ends_with("helper"));
        assert!(built.edges.iter().any(|e| e.relation == EdgeRelation::Defines));
        assert_eq!(built.graph.node_count(), pf.nodes.len());
    }

    fn build_ts(files: &[(&str, &str)]) -> Built {
        let (mut nodes, mut calls, mut inherits, mut fields, mut locals, mut imports) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (f, src) in files {
            let pf = codegraph_parse::parse_ts("p", f, src);
            nodes.extend(pf.nodes);
            calls.extend(pf.calls);
            inherits.extend(pf.inherits);
            fields.extend(pf.fields);
            locals.extend(pf.locals);
            imports.extend(pf.imports);
        }
        build(&nodes, &calls, &inherits, &fields, &locals, &imports)
    }

    fn build_swift(files: &[(&str, &str)]) -> Built {
        let (mut nodes, mut calls, mut inherits, mut fields, mut locals, mut imports) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (f, src) in files {
            let pf = codegraph_parse::parse_swift("p", f, src);
            nodes.extend(pf.nodes);
            calls.extend(pf.calls);
            inherits.extend(pf.inherits);
            fields.extend(pf.fields);
            locals.extend(pf.locals);
            imports.extend(pf.imports);
        }
        build(&nodes, &calls, &inherits, &fields, &locals, &imports)
    }

    #[test]
    fn swift_field_typed_call_resolves() {
        // `fetch` is ambiguous (HomeVM.fetch + OtherVM.fetch); self.vm.fetch()
        // resolves via the property's declared type (T3, FieldTypeMember).
        let built = build_swift(&[
            ("home_vm.swift", "class HomeVM {\n  func fetch() {}\n}"),
            ("other_vm.swift", "class OtherVM {\n  func fetch() {}\n}"),
            ("vc.swift", "class VC {\n  let vm: HomeVM\n  func go() { self.vm.fetch() }\n}"),
        ]);
        let fetch: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".fetch"))
            .collect();
        assert_eq!(fetch.len(), 1, "self.vm.fetch() resolves to exactly one method");
        assert!(fetch[0].dst.contains("home_vm"), "resolved to HomeVM.fetch, not OtherVM");
        assert_eq!(fetch[0].metadata.get("justification").and_then(|v| v.as_str()), Some("FieldTypeMember"));
    }

    #[test]
    fn swift_implicit_member_field_call_resolves() {
        // Swift writes `vm.fetch()` (implicit self), NOT `self.vm.fetch()`. `fetch`
        // is ambiguous, so it must resolve via the field's type (implicit member).
        let built = build_swift(&[
            ("home_vm.swift", "class HomeVM {\n  func fetch() {}\n}"),
            ("other_vm.swift", "class OtherVM {\n  func fetch() {}\n}"),
            ("vc.swift", "class VC {\n  let vm: HomeVM\n  func go() { vm.fetch() }\n}"),
        ]);
        let fetch: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".fetch"))
            .collect();
        assert_eq!(fetch.len(), 1, "implicit-member vm.fetch() resolves to one method");
        assert!(fetch[0].dst.contains("home_vm"), "resolved to HomeVM.fetch");
        assert_eq!(fetch[0].metadata.get("justification").and_then(|v| v.as_str()), Some("FieldTypeMember"));
    }

    #[test]
    fn swift_array_field_does_not_resolve() {
        // `[HomeVM]` is an array — self.vms.fetch() must NOT resolve to HomeVM.fetch.
        // `fetch` is ambiguous (HomeVM + OtherVM) so the ONLY way an edge appears is
        // a wrong array→element field-type resolution; assert it drops instead.
        let built = build_swift(&[
            ("home_vm.swift", "class HomeVM {\n  func fetch() {}\n}"),
            ("other_vm.swift", "class OtherVM {\n  func fetch() {}\n}"),
            ("vc.swift", "class VC {\n  let vms: [HomeVM]\n  func go() { self.vms.fetch() }\n}"),
        ]);
        let n = built.edges.iter().filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".fetch")).count();
        assert_eq!(n, 0, "array-typed field must not resolve an ambiguous call to the element type");
    }

    #[test]
    fn b1_same_named_methods_in_one_file_get_distinct_ids() {
        // Two classes in ONE file, both defining `bar`. Pre-B1 both got the same
        // id (project.file.bar) — one node, misattributed members. Now they nest
        // under their class ids, and this.bar() resolves to A's bar only.
        let built = build_ts(&[(
            "src/two.ts",
            "class A { foo() { this.bar(); } bar() {} }\nclass B { bar() {} }",
        )]);
        let pf = codegraph_parse::parse_ts("p", "src/two.ts", "class A { foo() { this.bar(); } bar() {} }\nclass B { bar() {} }");
        let bars: Vec<_> = pf.nodes.iter().filter(|n| n.name == "bar").collect();
        assert_eq!(bars.len(), 2, "two distinct bar nodes");
        assert_ne!(bars[0].id, bars[1].id, "ids must differ (class-qualified)");
        assert!(bars.iter().any(|n| n.id.contains(".a.")), "one nests under class a");
        assert!(bars.iter().any(|n| n.id.contains(".b.")), "one nests under class b");
        let this_bar: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".bar"))
            .collect();
        assert_eq!(this_bar.len(), 1, "this.bar() resolves to exactly one method");
        assert!(this_bar[0].dst.contains(".a."), "resolved to A.bar, not B.bar");
    }

    #[test]
    fn t5_local_var_typed_call_resolves() {
        // `save` is project-ambiguous (UserRepo.save + OtherRepo.save). A typed
        // local `const r: UserRepo` / a typed param resolves r.save() to UserRepo.save.
        let built = build_ts(&[
            ("user.repo.ts", "class UserRepo { save() {} }"),
            ("other.repo.ts", "class OtherRepo { save() {} }"),
            ("svc.ts", "class Svc { go(r: UserRepo) { r.save(); } }"),
        ]);
        let save: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".save"))
            .collect();
        assert_eq!(save.len(), 1, "typed local r.save() resolves to exactly one method");
        assert!(save[0].dst.contains("user_repo"), "resolved to UserRepo.save, not OtherRepo");
        assert_eq!(save[0].metadata.get("justification").and_then(|v| v.as_str()), Some("LocalVarType"));
    }

    #[test]
    fn t5_conflicting_local_type_drops_no_guess() {
        // `x` declared with two different types in one scope → poisoned → drop.
        let built = build_ts(&[
            ("a.ts", "class A { save() {} }"),
            ("b.ts", "class B { save() {} }"),
            ("svc.ts", "function go() { let x: A; let x: B; x.save(); }"),
        ]);
        let save = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".save"))
            .count();
        assert_eq!(save, 0, "conflicting local types must drop, not guess");
    }


    #[test]
    fn t6_import_narrowed_resolves_ambiguous_cross_file() {
        // `save` is defined in TWO files (globally ambiguous). The caller imports it
        // from './a' — the import is the evidence → resolves to a.ts's save.
        let built = build_ts(&[
            ("src/a.ts", "export function save() {}"),
            ("src/b.ts", "export function save() {}"),
            ("src/svc.ts", "import { save } from './a';\nexport function go() { save(); }"),
        ]);
        let hits: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".save"))
            .collect();
        assert_eq!(hits.len(), 1, "import-narrowed call resolves to exactly one def");
        assert!(hits[0].dst.contains(".a_ts."), "resolved to a.ts, not b.ts");
        assert_eq!(hits[0].metadata.get("justification").and_then(|v| v.as_str()), Some("ImportNarrowed"));
    }

    #[test]
    fn t6_no_import_ambiguous_still_drops() {
        // Same ambiguity, NO import → must stay dropped (no phantom edge).
        let built = build_ts(&[
            ("src/a.ts", "export function save() {}"),
            ("src/b.ts", "export function save() {}"),
            ("src/svc.ts", "export function go() { save(); }"),
        ]);
        let n = built.edges.iter().filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".save")).count();
        assert_eq!(n, 0, "ambiguous without evidence must drop");
    }

    #[test]
    fn every_call_edge_carries_a_justification_tag() {
        // Proof-obligation invariant (docs/RESOLUTION.md): no CALLS edge without a
        // justification — the per-tier measurement + precision-audit surface.
        let built = build_ts(&[
            ("a.ts", "class A { foo() { this.bar(); } bar() {} }"),
            ("b.ts", "class B { run() { helper(); } }\nfunction helper() {}"),
        ]);
        let call_edges: Vec<_> = built.edges.iter().filter(|e| e.relation == EdgeRelation::Calls).collect();
        assert!(!call_edges.is_empty());
        for e in &call_edges {
            let tag = e.metadata.get("justification").and_then(|v| v.as_str());
            assert!(tag.is_some(), "CALLS edge {}->{} has no justification tag", e.src, e.dst);
            assert!(
                ["SelfThisMember", "FieldTypeMember", "LocalVarType", "SameFileUnique", "ImportNarrowed", "PackageScope", "GlobalUnique"].contains(&tag.unwrap()),
                "unexpected justification tag: {:?}",
                tag
            );
        }
    }

    #[test]
    fn swift_self_call_resolves_to_class_method() {
        // `save` is project-ambiguous (A.save + B.save); self.save() in A must
        // resolve to A.save via T1 (the receiver type IS the enclosing class).
        let built = build_swift(&[
            ("a.swift", "class A {\n  func go() { self.save() }\n  func save() {}\n}"),
            ("b.swift", "class B {\n  func save() {}\n}"),
        ]);
        let save: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".save"))
            .collect();
        assert_eq!(save.len(), 1, "self.save() resolves to exactly one method");
        assert!(save[0].dst.contains("a_swift"), "must be A.save, not B.save");
    }

    #[test]
    fn qualified_named_receiver_ambiguous_name_drops_no_guess() {
        // obj.save() with an ambiguous `save` must NOT resolve to a same-file
        // `save` — the receiver's type is unknown, so it drops (precision guard).
        let built = build_swift(&[
            ("a.swift", "class A {\n  func go() { let obj = vm; obj.save() }\n  func save() {}\n}"),
            ("b.swift", "class B {\n  func save() {}\n}"),
        ]);
        let save_edges = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".save"))
            .count();
        assert_eq!(save_edges, 0, "ambiguous qualified obj.save() must drop, not guess a same-file member");
    }

    #[test]
    fn qualified_named_receiver_never_binds_by_name_uniqueness() {
        // obj.persistUniquely() with an UNKNOWN receiver type: the receiver's
        // existence is evidence the callee is a member of some type — binding
        // to a repo-unique method of an unrelated class is a guess. The audit
        // measured this exact pattern at 27% precision on a real repo. Drop.
        let built = build_swift(&[
            ("a.swift", "class A {\n  func go() { let obj = repo; obj.persistUniquely() }\n}"),
            ("b.swift", "class B {\n  func persistUniquely() {}\n}"),
        ]);
        let resolved = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".persistuniquely"))
            .count();
        assert_eq!(resolved, 0, "unknown-receiver call must drop, never bind by name uniqueness");
    }

    #[test]
    fn parameter_shadow_suppresses_bare_resolution() {
        // `call_next` is a PARAMETER of `handler` — calling it must not bind
        // the same-named free function defined elsewhere (audit-caught).
        let built = build_ts(&[
            ("mw.ts", "export function handler(call_next: Handler) { call_next(); }"),
            ("other.ts", "export function call_next() {}"),
        ]);
        let bound = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".call_next"))
            .count();
        assert_eq!(bound, 0, "a locally-bound name must never resolve to a free function");
    }

    #[test]
    fn t1_resolves_inherited_self_call_that_was_dropped() {
        // `bar` is project-ambiguous (Base.bar + Other.bar), so the old resolver
        // DROPS `this.bar()`. T1 resolves it to Base.bar via A's INHERITS chain.
        let built = build_ts(&[
            ("base.ts", "class Base { bar() {} }"),
            ("a.ts", "class A extends Base { foo() { this.bar(); } }"),
            ("other.ts", "class Other { bar() {} }"),
        ]);
        let bar_calls: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".bar"))
            .collect();
        assert_eq!(bar_calls.len(), 1, "exactly one resolved bar call");
        assert!(bar_calls[0].src.ends_with(".foo"));
        assert!(bar_calls[0].dst.contains("base"), "resolved to Base.bar, not Other.bar");
    }

    #[test]
    fn t1_drops_self_call_with_no_hierarchy_match_no_guess() {
        // `widget` is project-ambiguous and NOT on A or its ancestors: must DROP
        // (T1 returns None; T4 drops the ambiguous name) — precision preserved.
        let built = build_ts(&[
            ("a.ts", "class A { foo() { this.widget(); } }"),
            ("b.ts", "class B { widget() {} }"),
            ("c.ts", "class C { widget() {} }"),
        ]);
        let widget_calls = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".widget"))
            .count();
        assert_eq!(widget_calls, 0, "ambiguous self-call must not produce a phantom edge");
    }

    #[test]
    fn t3_resolves_field_typed_di_call() {
        // `find` is project-ambiguous; this.userService.find() resolves to
        // UserService.find via the constructor parameter-property's declared type.
        let built = build_ts(&[
            ("user.service.ts", "class UserService { find() {} }"),
            ("other.service.ts", "class OtherService { find() {} }"),
            ("app.ts", "class App { constructor(private userService: UserService) {} go() { this.userService.find(); } }"),
        ]);
        let find_calls: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".find"))
            .collect();
        assert_eq!(find_calls.len(), 1, "exactly one resolved find call");
        assert!(find_calls[0].src.ends_with(".go"));
        assert!(find_calls[0].dst.contains("user_service"), "resolved to UserService.find, not OtherService");
    }

    #[test]
    fn t3_drops_when_field_type_unknown_no_guess() {
        // `svc` has no typed declaration -> T3 can't resolve -> the ambiguous name
        // drops (no phantom edge).
        let built = build_ts(&[
            ("a.ts", "class UserService { find() {} }"),
            ("b.ts", "class OtherService { find() {} }"),
            ("app.ts", "class App { go() { this.svc.find(); } }"),
        ]);
        let find_calls = built
            .edges
            .iter()
            .filter(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with(".find"))
            .count();
        assert_eq!(find_calls, 0, "unknown field type must not guess");
    }

    #[test]
    fn implements_hyperedge_materializes() {
        let pf = codegraph_parse::parse_ts(
            "p", "a.ts",
            "interface Repo {}\nclass SqlRepo implements Repo {}\nclass MemRepo implements Repo {}\n",
        );
        let built = build(&pf.nodes, &pf.calls, &pf.inherits, &pf.fields, &pf.locals, &pf.imports);
        assert!(built.edges.iter().any(|e| e.relation == EdgeRelation::Implements));
        let he = built.hyperedges.iter().find(|(h, _)| h.label.contains("Repo")).expect("hyperedge");
        // 2 implementers + the interface = 3 members
        assert_eq!(he.1.len(), 3);
    }

    #[test]
    fn end_to_end_persist_and_query() {
        let pf = codegraph_parse::parse_rust("proj", "src/main.rs", "fn helper() {}\nfn main() { helper(); }\n");
        let built = build(&pf.nodes, &pf.calls, &pf.inherits, &pf.fields, &pf.locals, &pf.imports);
        let store = Store::open_in_memory().unwrap();
        for n in &pf.nodes {
            store.upsert_node(n).unwrap();
        }
        for e in &built.edges {
            store.upsert_edge(e).unwrap();
        }
        let main_id = pf.nodes.iter().find(|n| n.name == "main").unwrap().id.clone();
        let edges = store.get_edges_for_node(&main_id).unwrap();
        assert!(edges.iter().any(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with("helper")));
    }

    #[test]
    fn cross_file_unique_name_resolves() {
        // unique callee name across files -> cross-file CALLS edge
        let mut pf = codegraph_parse::parse_rust("proj", "a.rs", "fn main() { ghost(); }\n");
        let other = codegraph_parse::parse_rust("proj", "b.rs", "fn ghost() {}\n");
        pf.nodes.extend(other.nodes);
        pf.calls.extend(other.calls);
        let built = build(&pf.nodes, &pf.calls, &pf.inherits, &pf.fields, &pf.locals, &pf.imports);
        assert!(built.edges.iter().any(|e| e.relation == EdgeRelation::Calls
            && e.src.ends_with(".main") && e.dst.ends_with(".ghost")));
    }

    #[test]
    fn ambiguous_name_not_resolved_cross_file() {
        // same name defined in two files -> a call from a THIRD file stays unresolved
        let mut a = codegraph_parse::parse_rust("proj", "a.rs", "fn dup() {}\n");
        let b = codegraph_parse::parse_rust("proj", "b.rs", "fn dup() {}\n");
        let c = codegraph_parse::parse_rust("proj", "c.rs", "fn caller() { dup(); }\n");
        a.nodes.extend(b.nodes);
        a.nodes.extend(c.nodes);
        a.calls.extend(c.calls);
        let built = build(&a.nodes, &a.calls, &a.inherits, &a.fields, &a.locals, &a.imports);
        assert!(!built.edges.iter().any(|e| e.relation == EdgeRelation::Calls));
    }
}

/// Hub score shared by `important` and the report's community/central pickers:
/// `ln(1+fan_in) × ln(1+fan_out) + pagerank`. A symbol is core when it is both
/// widely CALLED and widely CALLING (orchestrates) — raw PageRank fails here
/// twice: pure-utility leaves (huge fan-in, fan-out ≤1) accumulate mass they
/// never shed, and their private helpers (fan-in 1) inherit that mass. The
/// degree product zeroes both failure modes; PageRank stays as the tiebreak
/// (it dominates only when a degree term is 0). Deterministic, threshold-free.
pub fn hub_score(pagerank: f64, fan_in: u32, fan_out: u32) -> f64 {
    (1.0 + fan_in as f64).ln() * (1.0 + fan_out as f64).ln() + pagerank
}

/// An in-memory graph loaded from the persisted store, with id↔index mapping,
/// for traversal and ranking queries (trace_path, blast-radius, callees, PageRank).
pub struct LoadedGraph {
    graph: CodeGraph,
    idx: HashMap<String, petgraph::stable_graph::NodeIndex>,
    ids: Vec<String>,
}

impl LoadedGraph {
    pub fn load(nodes: &[Node], edges: &[Edge]) -> Self {
        let mut graph = CodeGraph::new();
        let mut idx = HashMap::new();
        for n in nodes {
            idx.insert(n.id.clone(), graph.add_node(n.id.clone()));
        }
        for e in edges {
            if let (Some(&a), Some(&b)) = (idx.get(&e.src), idx.get(&e.dst)) {
                graph.add_edge(a, b, e.relation);
            }
        }
        let mut ids = vec![String::new(); graph.node_count()];
        for (id, ni) in &idx {
            ids[ni.index()] = id.clone();
        }
        LoadedGraph { graph, idx, ids }
    }

    /// Shortest dependency path (any edge) between two node ids, as an id list.
    pub fn shortest_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        let (s, g) = (*self.idx.get(from)?, *self.idx.get(to)?);
        let (_, path) = petgraph::algo::astar(&self.graph, s, |n| n == g, |_| 1, |_| 0)?;
        Some(path.into_iter().map(|ni| self.ids[ni.index()].clone()).collect())
    }

    /// Reverse reachability (who depends on `target`) up to `max_depth` hops.
    pub fn blast_radius(&self, target: &str, max_depth: usize) -> Vec<String> {
        let Some(&start) = self.idx.get(target) else { return Vec::new() };
        let mut visited: HashSet<_> = HashSet::from([start]);
        let mut frontier = vec![start];
        let mut out = Vec::new();
        for _ in 0..max_depth {
            let mut next = Vec::new();
            for &n in &frontier {
                for pred in self.graph.neighbors_directed(n, petgraph::Direction::Incoming) {
                    if visited.insert(pred) {
                        next.push(pred);
                        out.push(self.ids[pred.index()].clone());
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out
    }


    /// Forward reachability over CALLS edges from `start` (the flow body).
    pub fn flow_from(&self, start: &str, max_depth: usize) -> Vec<String> {
        use petgraph::visit::EdgeRef;
        let Some(&s) = self.idx.get(start) else { return Vec::new() };
        let mut visited: HashSet<_> = HashSet::from([s]);
        let mut frontier = vec![s];
        let mut out = Vec::new();
        for _ in 0..max_depth {
            let mut next = Vec::new();
            for &n in &frontier {
                for e in self.graph.edges(n).filter(|e| *e.weight() == EdgeRelation::Calls) {
                    let t = e.target();
                    if visited.insert(t) {
                        next.push(t);
                        out.push(self.ids[t.index()].clone());
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out
    }

    /// Direct callees (outgoing CALLS edges) of a node id.
    pub fn callees(&self, of: &str) -> Vec<String> {
        use petgraph::visit::EdgeRef;
        let Some(&n) = self.idx.get(of) else { return Vec::new() };
        self.graph
            .edges(n)
            .filter(|e| *e.weight() == EdgeRelation::Calls)
            .map(|e| self.ids[e.target().index()].clone())
            .collect()
    }

    /// Top-k most central nodes by PageRank (deterministic id tiebreaker).
    /// Personalized PageRank restarted at `seed_ids` (semantic/search hits), ranked
    /// over the RESOLVED call+inherit graph. The basis of `context`: it surfaces the
    /// symbols structurally closest to what the query is about — using real resolved
    /// edges, not aider's name-match. Deterministic (fixed iterations, no RNG).
    pub fn personalized_pagerank_top(&self, seed_ids: &[String], k: usize) -> Vec<(String, f64)> {
        let seeds: Vec<usize> = seed_ids.iter().filter_map(|id| self.idx.get(id).map(|ni| ni.index())).collect();
        let ranks = self.personalized_page_rank(&seeds);
        let mut scored: Vec<(String, f64)> = self
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), ranks.get(i).copied().unwrap_or(0.0)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        scored.truncate(k);
        scored
    }

    fn personalized_page_rank(&self, seeds: &[usize]) -> Vec<f64> {
        let n = self.graph.node_count();
        if n == 0 {
            return Vec::new();
        }
        // Teleport vector: uniform over the seed set (uniform over all if no seeds).
        let mut pvec = vec![0.0; n];
        if seeds.is_empty() {
            pvec.iter_mut().for_each(|x| *x = 1.0 / n as f64);
        } else {
            let w = 1.0 / seeds.len() as f64;
            for &s in seeds {
                if s < n {
                    pvec[s] = w;
                }
            }
        }
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &ni in self.idx.values() {
            let i = ni.index();
            for nb in self.graph.neighbors(ni) {
                out[i].push(nb.index());
            }
        }
        // Lower damping than global PageRank (0.85): for query-LOCAL context we
        // want the teleport-to-seeds mass to dominate, so results cluster around
        // what the query is about rather than drifting to globally-central infra.
        let d = 0.6;
        let mut rank = pvec.clone();
        for _ in 0..50 {
            let mut next: Vec<f64> = pvec.iter().map(|p| (1.0 - d) * p).collect();
            let mut dangling = 0.0;
            for (i, outs) in out.iter().enumerate() {
                if outs.is_empty() {
                    dangling += rank[i];
                    continue;
                }
                let share = d * rank[i] / outs.len() as f64;
                for &j in outs {
                    next[j] += share;
                }
            }
            // Dangling mass returns to the teleport vector → stays personalized.
            for (j, p) in pvec.iter().enumerate() {
                next[j] += d * dangling * p;
            }
            rank = next;
        }
        rank
    }

    pub fn pagerank_top(&self, k: usize) -> Vec<(String, f64)> {
        let ranks = self.page_rank();
        let mut scored: Vec<(String, f64)> = self
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), ranks.get(i).copied().unwrap_or(0.0)))
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
        });
        scored.truncate(k);
        scored
    }

    /// Agent-facing importance ranking: `hub_score` restricted to REAL code
    /// symbols (no File/Document rows). See `hub_score` for why raw PageRank
    /// surfaces utility leaves instead of the codebase's actual core.
    pub fn important(&self, k: usize, nodes: &[Node]) -> Vec<(String, f64)> {
        let keep: HashMap<&str, ()> = nodes
            .iter()
            .filter(|n| {
                matches!(
                    n.label,
                    NodeLabel::Function | NodeLabel::Method | NodeLabel::Class | NodeLabel::Interface | NodeLabel::Route
                )
            })
            .map(|n| (n.id.as_str(), ()))
            .collect();
        let ranks = self.page_rank();
        let mut scored: Vec<(String, f64)> = self
            .ids
            .iter()
            .enumerate()
            .filter(|(_, id)| keep.contains_key(id.as_str()))
            .map(|(i, id)| {
                let ni = self.idx[id];
                let fan_in = self
                    .graph
                    .edges_directed(ni, petgraph::Direction::Incoming)
                    .filter(|e| *e.weight() == EdgeRelation::Calls)
                    .count() as u32;
                let fan_out = self
                    .graph
                    .edges_directed(ni, petgraph::Direction::Outgoing)
                    .filter(|e| *e.weight() == EdgeRelation::Calls)
                    .count() as u32;
                (id.clone(), hub_score(ranks.get(i).copied().unwrap_or(0.0), fan_in, fan_out))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
        });
        scored.truncate(k);
        scored
    }

    /// Per-node analytics: (community id, PageRank, betweenness). Computed once
    /// over the whole graph; persisted onto each node at index time.
    pub fn analyze(&self) -> HashMap<String, (u32, f64, f64)> {
        let comm = self.communities();
        let ranks = self.page_rank();
        let betw = self.betweenness();
        let mut out = HashMap::new();
        for (i, id) in self.ids.iter().enumerate() {
            out.insert(
                id.clone(),
                (
                    comm.get(id).copied().unwrap_or(0),
                    ranks.get(i).copied().unwrap_or(0.0),
                    betw.get(id).copied().unwrap_or(0.0),
                ),
            );
        }
        out
    }

    /// O((V+E) * iters) PageRank (petgraph's is O(V^2)/iter). Index = node index.
    fn page_rank(&self) -> Vec<f64> {
        let n = self.graph.node_count();
        if n == 0 {
            return Vec::new();
        }
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &ni in self.idx.values() {
            let i = ni.index();
            for nb in self.graph.neighbors(ni) {
                out[i].push(nb.index());
            }
        }
        let d = 0.85;
        let base = (1.0 - d) / n as f64;
        let mut rank = vec![1.0 / n as f64; n];
        for _ in 0..50 {
            let mut next = vec![base; n];
            let mut dangling = 0.0;
            for (i, outs) in out.iter().enumerate() {
                if outs.is_empty() {
                    dangling += rank[i];
                    continue;
                }
                let share = d * rank[i] / outs.len() as f64;
                for &j in outs {
                    next[j] += share;
                }
            }
            let dang = d * dangling / n as f64;
            for x in next.iter_mut() {
                *x += dang;
            }
            rank = next;
        }
        rank
    }

    /// Deterministic one-level Louvain (modularity local-moving); edges treated
    /// as undirected, weight 1. Tie-break to the smaller community id.
    pub fn communities(&self) -> HashMap<String, u32> {
        let n = self.graph.node_count();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &ni in self.idx.values() {
            let i = ni.index();
            for d in [petgraph::Direction::Outgoing, petgraph::Direction::Incoming] {
                for nb in self.graph.neighbors_directed(ni, d) {
                    adj[i].push(nb.index());
                }
            }
        }
        let deg: Vec<f64> = adj.iter().map(|a| a.len() as f64).collect();
        let m2: f64 = deg.iter().sum();
        let mut comm: Vec<usize> = (0..n).collect();
        if m2 > 0.0 {
            let mut sigma_tot: Vec<f64> = deg.clone();
            let mut improved = true;
            let mut rounds = 0;
            while improved && rounds < 20 {
                improved = false;
                rounds += 1;
                for v in 0..n {
                    if adj[v].is_empty() {
                        continue;
                    }
                    let cv = comm[v];
                    sigma_tot[cv] -= deg[v];
                    let mut k_in: HashMap<usize, f64> = HashMap::new();
                    for &u in &adj[v] {
                        *k_in.entry(comm[u]).or_default() += 1.0;
                    }
                    let mut best_c = cv;
                    let mut best_gain = k_in.get(&cv).copied().unwrap_or(0.0) - deg[v] * sigma_tot[cv] / m2;
                    for (&c, &kin) in &k_in {
                        let gain = kin - deg[v] * sigma_tot[c] / m2;
                        if gain > best_gain + 1e-12 || ((gain - best_gain).abs() <= 1e-12 && c < best_c) {
                            best_gain = gain;
                            best_c = c;
                        }
                    }
                    sigma_tot[best_c] += deg[v];
                    if best_c != cv {
                        comm[v] = best_c;
                        improved = true;
                    }
                }
            }
        }
        let mut remap: HashMap<usize, u32> = HashMap::new();
        let mut next = 0u32;
        let mut out = HashMap::new();
        for (id, &cm) in self.ids.iter().zip(&comm) {
            let c = *remap.entry(cm).or_insert_with(|| {
                let x = next;
                next += 1;
                x
            });
            out.insert(id.clone(), c);
        }
        out
    }

    /// Brandes betweenness centrality. Exact for graphs up to 1500 nodes;
    /// above that, a bounded set of evenly-spaced seeded pivots (reusing all
    /// buffers across pivots). Skipped (all zero) for pathologically large graphs.
    pub fn betweenness(&self) -> HashMap<String, f64> {
        let n = self.graph.node_count();
        let mut out = HashMap::new();
        if n == 0 {
            return out;
        }
        if n > 200_000 {
            for id in &self.ids {
                out.insert(id.clone(), 0.0);
            }
            return out;
        }
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &ni in self.idx.values() {
            let i = ni.index();
            for nb in self.graph.neighbors(ni) {
                adj[i].push(nb.index());
            }
        }
        let pivots: Vec<usize> = if n <= 1500 {
            (0..n).collect()
        } else {
            let k = 128usize.min(n);
            (0..n).step_by((n / k).max(1)).collect()
        };
        let mut bc = vec![0.0f64; n];
        let mut pred: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut sigma = vec![0.0f64; n];
        let mut dist = vec![-1i64; n];
        let mut delta = vec![0.0f64; n];
        let mut stack: Vec<usize> = Vec::with_capacity(n);
        let mut q = std::collections::VecDeque::new();
        for &s in &pivots {
            for pp in pred.iter_mut() {
                pp.clear();
            }
            sigma.iter_mut().for_each(|x| *x = 0.0);
            dist.iter_mut().for_each(|x| *x = -1);
            delta.iter_mut().for_each(|x| *x = 0.0);
            stack.clear();
            q.clear();
            sigma[s] = 1.0;
            dist[s] = 0;
            q.push_back(s);
            while let Some(v) = q.pop_front() {
                stack.push(v);
                for &w in &adj[v] {
                    if dist[w] < 0 {
                        dist[w] = dist[v] + 1;
                        q.push_back(w);
                    }
                    if dist[w] == dist[v] + 1 {
                        sigma[w] += sigma[v];
                        pred[w].push(v);
                    }
                }
            }
            while let Some(w) = stack.pop() {
                for &v in &pred[w] {
                    delta[v] += (sigma[v] / sigma[w]) * (1.0 + delta[w]);
                }
                if w != s {
                    bc[w] += delta[w];
                }
            }
        }
        for (id, &b) in self.ids.iter().zip(&bc) {
            out.insert(id.clone(), b);
        }
        out
    }
}

#[cfg(test)]
mod traversal_tests {
    use super::*;

    fn fixture() -> (Vec<Node>, Vec<Edge>) {
        let pf = codegraph_parse::parse_rust(
            "p",
            "src/lib.rs",
            "fn a() { b(); }\nfn b() { c(); }\nfn c() {}\nfn lonely() {}\n",
        );
        let built = build(&pf.nodes, &pf.calls, &pf.inherits, &pf.fields, &pf.locals, &pf.imports);
        (pf.nodes, built.edges)
    }

    #[test]
    fn shortest_path_and_blast_radius() {
        let (nodes, edges) = fixture();
        let lg = LoadedGraph::load(&nodes, &edges);
        let a = nodes.iter().find(|n| n.name == "a").unwrap().id.clone();
        let c = nodes.iter().find(|n| n.name == "c").unwrap().id.clone();
        let path = lg.shortest_path(&a, &c).unwrap();
        assert_eq!(path.first().unwrap(), &a);
        assert_eq!(path.last().unwrap(), &c);
        // who depends on c? a and b (transitively) plus the File via DEFINES
        let blast = lg.blast_radius(&c, 5);
        assert!(blast.iter().any(|id| id.ends_with(".b")));
        assert!(blast.iter().any(|id| id.ends_with(".a")));
    }

    #[test]
    fn communities_and_betweenness() {
        use codegraph_core::{Confidence, EdgeRelation, Metadata, NodeLabel, ResolutionTier};
        let mk_n = |x: &str| Node {
            id: x.into(), label: NodeLabel::Function, name: x.into(), file_path: "f".into(),
            line_start: 1, line_end: 1, language: "rust".into(), metadata: Metadata::new(),
            community: None, pagerank: 0.0, betweenness: 0.0,
        };
        let nodes: Vec<Node> = ["a", "b", "c", "d", "e", "f"].iter().map(|x| mk_n(x)).collect();
        let mk_e = |s: &str, d: &str| Edge {
            src: s.into(), dst: d.into(), relation: EdgeRelation::Calls, tier: ResolutionTier::TreeSitter,
            confidence: Confidence::Inferred, src_file: "f".into(), src_line: 1, metadata: Metadata::new(),
        };
        // two triangles {a,b,c} and {d,e,f} bridged by c->d
        let edges = vec![mk_e("a","b"), mk_e("b","c"), mk_e("c","a"), mk_e("c","d"), mk_e("d","e"), mk_e("e","f"), mk_e("f","d")];
        let lg = LoadedGraph::load(&nodes, &edges);
        let a = lg.analyze();
        let comms: std::collections::HashSet<u32> = a.values().map(|v| v.0).collect();
        assert!(comms.len() >= 2, "expected at least two communities");
        assert!(a["c"].2 > 0.0 || a["d"].2 > 0.0, "bridge node should have betweenness");
    }

    #[test]
    fn personalized_pagerank_localizes_and_is_deterministic() {
        let mk_n = |x: &str| Node {
            id: x.into(), label: NodeLabel::Function, name: x.into(), file_path: "f".into(),
            line_start: 1, line_end: 1, language: "rust".into(), metadata: Metadata::new(),
            community: None, pagerank: 0.0, betweenness: 0.0,
        };
        let mk_e = |s: &str, d: &str| Edge {
            src: s.into(), dst: d.into(), relation: EdgeRelation::Calls, tier: ResolutionTier::TreeSitter,
            confidence: Confidence::Inferred, src_file: "f".into(), src_line: 1, metadata: Metadata::new(),
        };
        // two disconnected components: a->b->c and x->y->z
        let nodes: Vec<Node> = ["a", "b", "c", "x", "y", "z"].iter().map(|s| mk_n(s)).collect();
        let edges = vec![mk_e("a", "b"), mk_e("b", "c"), mk_e("x", "y"), mk_e("y", "z")];
        let lg = LoadedGraph::load(&nodes, &edges);
        let top = lg.personalized_pagerank_top(&["a".to_string()], 6);
        let score = |id: &str| top.iter().find(|(i, _)| i == id).map(|(_, s)| *s).unwrap_or(0.0);
        // seeding at `a` must rank its reachable component above the disconnected one
        assert!(score("a") > score("x"), "seed outranks unrelated node");
        assert!(score("b") > score("z"), "seed's reachable neighbor outranks unrelated node");
        // determinism: identical seeds → bit-identical ranking
        assert_eq!(top, lg.personalized_pagerank_top(&["a".to_string()], 6));
    }

    #[test]
    fn analyze_is_deterministic() {
        // Same graph analyzed twice must be byte-identical (community + centrality),
        // or two developers indexing the same commit would get different results.
        let (nodes, edges) = fixture();
        let a = LoadedGraph::load(&nodes, &edges).analyze();
        let b = LoadedGraph::load(&nodes, &edges).analyze();
        assert_eq!(a.len(), b.len());
        for (id, va) in &a {
            let vb = b.get(id).expect("same node set");
            assert_eq!(va.0, vb.0, "community id must be stable for {id}");
            assert_eq!(va.1.to_bits(), vb.1.to_bits(), "pagerank must be bit-identical for {id}");
            assert_eq!(va.2.to_bits(), vb.2.to_bits(), "betweenness must be bit-identical for {id}");
        }
    }

    #[test]
    fn callees_and_pagerank() {
        let (nodes, edges) = fixture();
        let lg = LoadedGraph::load(&nodes, &edges);
        let a = nodes.iter().find(|n| n.name == "a").unwrap().id.clone();
        let callees = lg.callees(&a);
        assert!(callees.iter().any(|id| id.ends_with(".b")));
        let top = lg.pagerank_top(3);
        assert_eq!(top.len(), 3);
    }
}
