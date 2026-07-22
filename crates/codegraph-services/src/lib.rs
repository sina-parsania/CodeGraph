//! Application services — the ONE home for review/dead-code business logic.
//! CLI and MCP are thin adapters over these; the Store stays a persistence
//! layer. Semantic operations are ID-first (`NodeId`), evidence classes stay
//! separated, and every answer carries its generation + quality.

use codegraph_core::{
    AnswerMetadata, EvidenceQuality, GraphAnswer, GraphFreshness, GraphGeneration, Node, NodeId,
};
use codegraph_store::Store;
use std::path::Path;

pub use codegraph_core::TestEvidence;

/// One reviewed symbol row — everything both adapters need, nothing rendered.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReviewRow {
    pub id: NodeId,
    pub name: String,
    pub file: String,
    pub line: u32,
    /// Resolved incoming CALLS edges to THIS id.
    pub fan_in_resolved: usize,
    /// Textual call sites naming this symbol (name-level, separate evidence).
    pub textual_sites: usize,
    pub complexity: u64,
    pub tested: TestEvidence,
    pub risk: f64,
    pub tier: RiskTier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum RiskTier {
    High,
    Med,
    Low,
}

/// What the review can know about one changed file, layered from graph
/// manifest/index metadata + git status + the indexer's extension policy —
/// never from `Path::exists()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileGraphCoverage {
    IndexedWithSymbols,
    IndexedWithoutSymbols,
    NotIndexed,
    Deleted,
    Unsupported,
}

/// A symbol that exists only on the BASE side of the diff (deleted file,
/// removed from a modified file, or lost across a rename). Its current-graph
/// fan-in/test evidence cannot be reconstructed — that is typed, per symbol,
/// never collapsed into a bare file path.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BaseOnlySymbol {
    pub name: String,
    pub kind: codegraph_core::NodeLabel,
    /// base-side path
    pub file: String,
    pub line: u32,
    pub disposition: BaseDisposition,
    pub evidence: EvidenceQuality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseDisposition {
    /// The whole file was deleted.
    FileDeleted,
    /// The file still exists; this symbol was removed from it.
    RemovedFromFile,
    /// The file was renamed/copied and this symbol is absent on the new side.
    LostInRename,
}

/// The full review result: ranked rows + base-side symbols reviewed at
/// symbol granularity + files the graph cannot see (unindexed/parse-failed —
/// UNKNOWN, so risk gates never pass on missing data) + co-change hints.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Review {
    pub base: String,
    pub changed_files: Vec<String>,
    pub rows: Vec<ReviewRow>,
    /// Symbols visible only at `base` — deleted/removed/renamed-away.
    pub base_only: Vec<BaseOnlySymbol>,
    pub unknown_files: Vec<String>,
    pub co_change_hints: Vec<(String, u32)>,
}

/// One changed path from `git diff --name-status -z --find-renames`.
#[derive(Debug, Clone)]
pub struct ChangedPath {
    pub kind: ChangeKind,
    /// current-side path (new name for renames/copies)
    pub path: String,
    /// base-side path for renames/copies
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

pub struct ReviewService;

impl ReviewService {
    /// Review the git diff vs `base`. Git failures PROPAGATE (an invalid ref
    /// must never read as "no changes"); metrics key on node IDs.
    /// `is_code_file` abstracts the indexer's extension policy so the service
    /// stays store-pure.
    pub fn review(
        store: &Store,
        root: &Path,
        base: &str,
        is_code_file: impl Fn(&str) -> bool,
    ) -> anyhow::Result<GraphAnswer<Review>> {
        let changes = Self::diff_status(root, base)?;
        let mut rows = Vec::new();
        let mut base_only: Vec<BaseOnlySymbol> = Vec::new();
        let mut unknown = Vec::new();
        let mut changed = Vec::new();
        for ch in &changes {
            changed.push(ch.path.clone());
            // The path whose BASE-side content is authoritative for this
            // change (old name for renames/copies, same name otherwise).
            let base_path = ch.old_path.as_deref().unwrap_or(&ch.path);
            if ch.kind == ChangeKind::Deleted {
                // A deleted doc/config/image is simply not reviewable code.
                // A deleted SOURCE file is reviewed at SYMBOL granularity
                // from `git show <base>:<path>` — never collapsed to a path.
                if is_code_file(&ch.path) {
                    match Self::base_symbols(root, base, &ch.path) {
                        Ok(syms) if !syms.is_empty() => {
                            base_only.extend(syms.into_iter().map(|(name, kind, line)| {
                                Self::base_only_symbol(
                                    name,
                                    kind,
                                    &ch.path,
                                    line,
                                    BaseDisposition::FileDeleted,
                                )
                            }));
                        }
                        Ok(_) => {} // legitimately symbol-free at base too
                        Err(e) => {
                            unknown.push(format!("{} (base source unavailable: {e})", ch.path))
                        }
                    }
                }
                continue;
            }
            let syms = store.symbols_in_file(&ch.path)?;
            let current_names: std::collections::BTreeSet<String> =
                syms.iter().map(|s| s.name.clone()).collect();
            if syms.is_empty() && is_code_file(&ch.path) {
                // Coverage comes from the GRAPH (manifest/index metadata),
                // never Path::exists(): an existing-but-unindexed or
                // skipped/unparsable file is missing coverage, not
                // "legitimately symbol-free".
                match store.file_graph_coverage(&ch.path)? {
                    codegraph_store::StoreFileCoverage::IndexedWithoutSymbols => {
                        // The graph indexed it and found nothing. Cross-check
                        // with a fresh parse of the CURRENT source: if the
                        // parser sees symbols the graph lacks, coverage is
                        // stale/broken → UNKNOWN, not a clean pass.
                        match std::fs::read_to_string(root.join(&ch.path)) {
                            Ok(src) => {
                                let fresh = codegraph_parse::parse_file("review", &ch.path, &src);
                                if fresh.nodes.iter().any(|n| Self::is_symbol_label(n.label)) {
                                    unknown.push(format!(
                                        "{} (indexed without symbols but the parser finds some — graph coverage is incomplete)",
                                        ch.path
                                    ));
                                }
                            }
                            Err(e) => unknown
                                .push(format!("{} (unreadable for verification: {e})", ch.path)),
                        }
                    }
                    codegraph_store::StoreFileCoverage::NotIndexed => {
                        unknown.push(format!("{} (not indexed — no graph coverage)", ch.path));
                    }
                    codegraph_store::StoreFileCoverage::IndexedWithSymbols => {
                        unreachable!("symbols_in_file returned empty for IndexedWithSymbols")
                    }
                }
            }
            for sym in syms {
                rows.push(Self::row_for(store, &sym)?);
            }
            // BASE side of modified/renamed/copied CODE: detect symbols that
            // existed at base and are gone on the current side. Runs even
            // when the new side has no symbols at all (a rename to a
            // symbol-free file must still review the old side). The
            // comparison is PARSER-vs-PARSER: both sides go through the same
            // parse, so a graph-side emission quirk can never fabricate a
            // "removed" symbol (union with graph names for extra safety).
            if is_code_file(base_path) && ch.kind != ChangeKind::Added {
                let disposition = match ch.kind {
                    ChangeKind::Renamed | ChangeKind::Copied => BaseDisposition::LostInRename,
                    _ => BaseDisposition::RemovedFromFile,
                };
                let mut present = current_names.clone();
                match std::fs::read_to_string(root.join(&ch.path)) {
                    Ok(src) => {
                        for n in codegraph_parse::parse_file("review", &ch.path, &src).nodes {
                            if Self::is_symbol_label(n.label) {
                                present.insert(n.name);
                            }
                        }
                    }
                    Err(e) => {
                        unknown.push(format!("{} (unreadable for base comparison: {e})", ch.path));
                        continue;
                    }
                }
                match Self::base_symbols(root, base, base_path) {
                    Ok(syms) => {
                        for (name, kind, line) in syms {
                            if !present.contains(&name) {
                                base_only.push(Self::base_only_symbol(
                                    name,
                                    kind,
                                    base_path,
                                    line,
                                    disposition,
                                ));
                            }
                        }
                    }
                    Err(e) => unknown.push(format!("{base_path} (base source unavailable: {e})")),
                }
            }
        }
        rows.sort_by(|a, b| {
            b.risk
                .partial_cmp(&a.risk)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut hints: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
        for f in &changed {
            for (other, n) in store.cochanges_for(f, 5)? {
                if n >= 3 && !changed.contains(&other) {
                    let e = hints.entry(other).or_insert(0);
                    *e = (*e).max(n);
                }
            }
        }
        let evidence = if unknown.is_empty() {
            EvidenceQuality::Exact
        } else {
            EvidenceQuality::LowerBound {
                reason: format!(
                    "{} changed path(s) have no current-graph view (deleted/renamed source)",
                    unknown.len()
                ),
            }
        };
        Ok(GraphAnswer {
            meta: metadata_for(store, evidence),
            data: Review {
                base: base.to_string(),
                changed_files: changed,
                rows,
                base_only,
                unknown_files: unknown,
                co_change_hints: hints.into_iter().collect(),
            },
        })
    }

    /// Base-side symbols of one path, parsed from `git show <base>:<path>`
    /// with the SAME parser the indexer uses. Errors (bad ref, path absent at
    /// base) propagate — the caller decides whether that is a coverage gap.
    fn base_symbols(
        root: &Path,
        base: &str,
        path: &str,
    ) -> anyhow::Result<Vec<(String, codegraph_core::NodeLabel, u32)>> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("show")
            .arg(format!("{base}:{path}"))
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git show {base}:{path} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let src = String::from_utf8_lossy(&out.stdout);
        let parsed = codegraph_parse::parse_file("base", path, &src);
        Ok(parsed
            .nodes
            .into_iter()
            .filter(|n| Self::is_symbol_label(n.label))
            .map(|n| (n.name, n.label, n.line_start))
            .collect())
    }

    /// The label set `symbols_in_file` reviews — File/Module/Document
    /// containers are not reviewable symbols.
    fn is_symbol_label(label: codegraph_core::NodeLabel) -> bool {
        matches!(
            label,
            codegraph_core::NodeLabel::Function
                | codegraph_core::NodeLabel::Method
                | codegraph_core::NodeLabel::Class
                | codegraph_core::NodeLabel::Interface
                | codegraph_core::NodeLabel::Enum
                | codegraph_core::NodeLabel::Type
        )
    }

    fn base_only_symbol(
        name: String,
        kind: codegraph_core::NodeLabel,
        file: &str,
        line: u32,
        disposition: BaseDisposition,
    ) -> BaseOnlySymbol {
        BaseOnlySymbol {
            name,
            kind,
            file: file.to_string(),
            line,
            disposition,
            // typed per-symbol unknown: the current graph holds no fan-in or
            // test evidence for a symbol that no longer exists in it
            evidence: EvidenceQuality::Unavailable {
                reason: "base-side symbol — fan-in/test evidence not reconstructible from the current graph"
                    .into(),
            },
        }
    }

    /// NUL-delimited `--name-status` parsing: robust for spaces, tabs and
    /// non-ASCII in paths; models A/M/D/R/C explicitly with both sides of a
    /// rename/copy. Git failures PROPAGATE — an invalid ref must never read
    /// as "no changes".
    fn diff_status(root: &Path, base: &str) -> anyhow::Result<Vec<ChangedPath>> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "diff",
                "--name-status",
                "-z",
                "--find-renames",
                "--find-copies",
                base,
                "--",
            ])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git diff vs '{base}' failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let mut fields = out
            .stdout
            .split(|&b| b == 0)
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .filter(|f| !f.is_empty())
            .collect::<Vec<_>>()
            .into_iter();
        let mut changes = Vec::new();
        while let Some(status) = fields.next() {
            let kind = match status.chars().next() {
                Some('A') => ChangeKind::Added,
                Some('M') | Some('T') => ChangeKind::Modified,
                Some('D') => ChangeKind::Deleted,
                Some('R') => ChangeKind::Renamed,
                Some('C') => ChangeKind::Copied,
                other => anyhow::bail!("unrecognized git status field {status:?} ({other:?})"),
            };
            let first = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("git -z stream truncated after {status:?}"))?;
            let (old_path, path) = if matches!(kind, ChangeKind::Renamed | ChangeKind::Copied) {
                let new = fields
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("rename/copy missing new path"))?;
                (Some(first), new)
            } else {
                (None, first)
            };
            changes.push(ChangedPath {
                kind,
                path,
                old_path,
            });
        }
        Ok(changes)
    }

    pub(crate) fn row_for(store: &Store, sym: &Node) -> anyhow::Result<ReviewRow> {
        let id = NodeId::new(sym.id.clone()).map_err(anyhow::Error::msg)?;
        let fan_in = store.fan_in_of_id(id.as_str())?;
        let textual = store.call_site_count(&sym.name)?;
        let tested = store.test_evidence_of_id(id.as_str(), &sym.name)?;
        let cx = sym
            .metadata
            .get("complexity")
            .and_then(|v| v.as_u64())
            .unwrap_or(1);
        // Multiplicative risk: reach × intrinsic complexity × untested penalty.
        // Only RESOLVED coverage clears the penalty — textual mentions are
        // name-level evidence and may belong to a same-name sibling.
        let risk = (1.0 + fan_in as f64).ln().max(0.35)
            * (1.0 + cx as f64 / 10.0)
            * if tested == TestEvidence::Resolved {
                1.0
            } else {
                2.0
            };
        let tier = if risk >= 6.0 {
            RiskTier::High
        } else if risk >= 2.5 {
            RiskTier::Med
        } else {
            RiskTier::Low
        };
        Ok(ReviewRow {
            id,
            name: sym.name.clone(),
            file: sym.file_path.clone(),
            line: sym.line_start,
            fan_in_resolved: fan_in,
            textual_sites: textual,
            complexity: cx,
            tested,
            risk,
            tier,
        })
    }
}

/// Typed dead-code verdict — "no evidence at all", "indirectly referenced",
/// "excluded by context" and "cannot know" are DIFFERENT answers. A symbol
/// with any indirect evidence is never presented as a certain candidate.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "assessment", rename_all = "snake_case")]
pub enum DeadCodeAssessment {
    /// No resolved edge, no textual call site, no typed reference: the
    /// strongest static "likely dead" the graph can state.
    Candidate { node: LeanSym },
    /// Name-level non-call evidence exists (callback, function value, macro
    /// or framework registration, FFI export) — a lead, not a candidate.
    IndirectlyReferenced {
        node: LeanSym,
        kinds: Vec<codegraph_core::ReferenceKind>,
        sites: usize,
    },
    /// Deliberately out of scope (entrypoint naming conventions).
    Excluded { node: LeanSym, context: String },
    /// Liveness is genuinely unknowable statically (public export with no
    /// in-repo evidence, or only unclassifiable reference positions).
    Unknown { node: LeanSym, reason: String },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LeanSym {
    pub id: NodeId,
    pub name: String,
    pub kind: codegraph_core::NodeLabel,
    pub file: String,
    pub line: u32,
}

pub struct DeadCodeService;

const ENTRYPOINT_NAMES: &[&str] = &["main", "init", "new", "setup", "run", "constructor"];

impl DeadCodeService {
    /// Typed assessments for the first `limit` symbols (deterministic
    /// file/line order) that have NO direct call evidence. Store supplies
    /// indexed primitives; the assessment semantics live HERE, shared by CLI
    /// and MCP.
    pub fn assess(
        store: &Store,
        limit: usize,
    ) -> anyhow::Result<GraphAnswer<Vec<DeadCodeAssessment>>> {
        let nodes = store.dead_code_batch(limit, 0)?;
        let data = nodes
            .into_iter()
            .map(|n| Self::classify(store, n))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(GraphAnswer {
            meta: Self::meta(store),
            data,
        })
    }

    /// CANDIDATES only (the v1 list): batches through the no-direct-evidence
    /// set until `limit` candidates are collected or the set is exhausted, so
    /// indirectly-referenced symbols never eat into the requested count.
    pub fn candidates(store: &Store, limit: usize) -> anyhow::Result<GraphAnswer<Vec<LeanSym>>> {
        let mut out = Vec::new();
        let batch = limit.max(200);
        let mut offset = 0;
        loop {
            let nodes = store.dead_code_batch(batch, offset)?;
            let exhausted = nodes.len() < batch;
            offset += nodes.len();
            for n in nodes {
                if let DeadCodeAssessment::Candidate { node } = Self::classify(store, n)? {
                    out.push(node);
                    if out.len() >= limit {
                        return Ok(GraphAnswer {
                            meta: Self::meta(store),
                            data: out,
                        });
                    }
                }
            }
            if exhausted {
                return Ok(GraphAnswer {
                    meta: Self::meta(store),
                    data: out,
                });
            }
        }
    }

    fn meta(store: &Store) -> codegraph_core::AnswerMetadata {
        // Static analysis can't see dynamic dispatch/reflection/exports:
        // candidates are a lower bound on liveness knowledge, by contract.
        metadata_for(
            store,
            EvidenceQuality::LowerBound {
                reason: "static view — dynamic dispatch, exports and reflection are invisible"
                    .into(),
            },
        )
    }

    /// Evidence-ordered classification for ONE symbol with no direct call
    /// evidence. Definition-site kinds (FFI/public/framework) must come from
    /// the symbol's OWN file — a same-name twin elsewhere never lends its
    /// export surface. Usage-site kinds (callback/value/macro) are name-level
    /// by nature and say so via their type.
    fn classify(store: &Store, n: Node) -> anyhow::Result<DeadCodeAssessment> {
        use codegraph_core::ReferenceKind as RK;
        let node = LeanSym {
            id: NodeId::new(n.id).map_err(anyhow::Error::msg)?,
            name: n.name.clone(),
            kind: n.label,
            file: n.file_path.clone(),
            line: n.line_start,
        };
        if ENTRYPOINT_NAMES.contains(&n.name.as_str()) {
            return Ok(DeadCodeAssessment::Excluded {
                node,
                context: "entrypoint naming convention (main/init/new/setup/run/constructor)"
                    .into(),
            });
        }
        let refs = store.refs_for_leaf(codegraph_store::leaf_name(&n.name))?;
        let mut kinds: Vec<RK> = Vec::new();
        let mut sites = 0usize;
        for (kind, file) in refs {
            let counts = match kind {
                // definition-site surface: only the symbol's own file
                RK::FfiExport | RK::PublicExport | RK::FrameworkRegistration => file == n.file_path,
                // usage-site evidence: name-level, any file
                RK::Callback
                | RK::FunctionValue
                | RK::MacroRegistration
                | RK::UnknownIndirect
                | RK::DirectCall => true,
            };
            if counts {
                sites += 1;
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
        }
        if store.is_route_handler(&n.name)? && !kinds.contains(&RK::FrameworkRegistration) {
            kinds.push(RK::FrameworkRegistration);
            sites += 1;
        }
        if kinds.is_empty() {
            return Ok(DeadCodeAssessment::Candidate { node });
        }
        if kinds.iter().all(|k| *k == RK::UnknownIndirect) {
            return Ok(DeadCodeAssessment::Unknown {
                node,
                reason: format!(
                    "referenced only in positions static analysis cannot classify ({sites} site(s))"
                ),
            });
        }
        if kinds.iter().all(|k| *k == RK::PublicExport) {
            return Ok(DeadCodeAssessment::Unknown {
                node,
                reason: "public export with no in-repo references — external callers are invisible"
                    .into(),
            });
        }
        Ok(DeadCodeAssessment::IndirectlyReferenced { node, kinds, sites })
    }
}

/// Graph generation, PROPAGATING failures: a SQLite error or a malformed
/// stamp is a provenance failure the caller must surface. A MISSING stamp is
/// `Ok(None)` — a legacy pre-generation graph, never invented as generation 0.
fn generation_of(store: &Store) -> anyhow::Result<Option<GraphGeneration>> {
    let raw = store
        .meta_get("generation")
        .map_err(|e| anyhow::anyhow!("provenance unavailable: {e}"))?;
    match raw {
        None => Ok(None),
        Some(g) => g
            .parse()
            .map(|n| Some(GraphGeneration(n)))
            .map_err(|e| anyhow::anyhow!("malformed generation stamp {g:?}: {e}")),
    }
}

/// Metadata for an answer computed against the CURRENT graph snapshot. The
/// adapter (CLI/MCP) knows whether the snapshot was refreshed before serving
/// and stamps freshness itself; here freshness only degrades to `Unknown`
/// when provenance itself is missing/broken:
/// - valid stamp    → generation Some, evidence as computed;
/// - missing stamp  → generation None + freshness Unknown (a legacy graph
///   can't prove which snapshot answered) — never invented generation 0;
/// - malformed/SQL error → generation None, evidence Unavailable.
pub fn metadata_for(store: &Store, evidence: EvidenceQuality) -> AnswerMetadata {
    match generation_of(store) {
        Ok(Some(g)) => AnswerMetadata {
            generation: Some(g),
            freshness: GraphFreshness::Fresh,
            evidence,
            coverage: None,
        },
        Ok(None) => AnswerMetadata {
            generation: None,
            freshness: GraphFreshness::Unknown {
                reason: "graph carries no generation stamp (legacy index)".into(),
            },
            evidence,
            coverage: None,
        },
        Err(e) => AnswerMetadata {
            generation: None,
            freshness: GraphFreshness::Unknown {
                reason: e.to_string(),
            },
            evidence: EvidenceQuality::Unavailable {
                reason: e.to_string(),
            },
            coverage: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P2: generation provenance is never invented. Missing stamp → `None` +
    /// Unknown freshness (a legacy graph cannot claim to be "simply Exact");
    /// valid stamp → typed generation; malformed stamp (same code path as a
    /// SQLite meta failure) → evidence Unavailable.
    #[test]
    fn generation_provenance_is_never_invented() {
        let store = Store::open_in_memory().unwrap();
        // missing stamp — legacy graph
        let m = metadata_for(&store, EvidenceQuality::Exact);
        assert_eq!(m.generation, None, "missing stamp must not become Some(0)");
        assert!(
            matches!(m.freshness, GraphFreshness::Unknown { .. }),
            "unknown provenance must not read as a plain Fresh+Exact answer: {m:?}"
        );
        assert_eq!(m.evidence, EvidenceQuality::Exact, "evidence is orthogonal");

        // valid stamp
        store.meta_set("generation", "7").unwrap();
        let m = metadata_for(&store, EvidenceQuality::Exact);
        assert_eq!(m.generation, Some(GraphGeneration(7)));
        assert_eq!(m.freshness, GraphFreshness::Fresh);

        // malformed stamp → provenance failure, evidence Unavailable
        store.meta_set("generation", "banana").unwrap();
        let m = metadata_for(&store, EvidenceQuality::Exact);
        assert_eq!(m.generation, None);
        assert!(
            matches!(m.evidence, EvidenceQuality::Unavailable { .. }),
            "malformed stamp must degrade evidence to Unavailable: {m:?}"
        );
    }

    fn fn_node(id: &str, name: &str, file: &str, line: u32) -> codegraph_core::Node {
        codegraph_core::Node {
            id: id.into(),
            label: codegraph_core::NodeLabel::Function,
            name: name.into(),
            file_path: file.into(),
            line_start: line,
            line_end: line + 1,
            language: "rust".into(),
            metadata: codegraph_core::Metadata::new(),
            community: None,
            pagerank: 0.0,
            betweenness: 0.0,
        }
    }

    /// P1 dead-code: every assessment class is produced from typed evidence,
    /// and definition-site kinds never leak to a same-name twin in another
    /// file.
    #[test]
    fn dead_code_assessments_are_typed_and_twin_safe() {
        use codegraph_core::{RawRef, ReferenceKind as RK};
        let store = Store::open_in_memory().unwrap();
        store
            .bulk_upsert_nodes(&[
                fn_node("p.a.cb_target", "cb_target", "a.rs", 1),
                fn_node("p.a.val_target", "val_target", "a.rs", 5),
                fn_node("p.a.pub_api", "pub_api", "a.rs", 9),
                fn_node("p.b.pub_api", "pub_api", "b.rs", 3), // twin, NOT pub
                fn_node("p.a.mystery", "mystery", "a.rs", 13),
                fn_node("p.c.truly_dead", "truly_dead", "c.rs", 1),
                fn_node("p.c.main", "main", "c.rs", 10),
                fn_node("p.c.handler_fn", "handler_fn", "c.rs", 20),
            ])
            .unwrap();
        // a Route node registering handler_fn
        let mut route = fn_node("p.r.route", "GET /x", "r.rs", 1);
        route.label = codegraph_core::NodeLabel::Route;
        route
            .metadata
            .insert("handler".into(), serde_json::json!("handler_fn"));
        store.bulk_upsert_nodes(&[route]).unwrap();
        store
            .save_refs(
                "a.rs",
                &[
                    RawRef {
                        name: "cb_target".into(),
                        line: 30,
                        kind: RK::Callback,
                    },
                    RawRef {
                        name: "val_target".into(),
                        line: 31,
                        kind: RK::FunctionValue,
                    },
                    RawRef {
                        name: "pub_api".into(),
                        line: 9,
                        kind: RK::PublicExport,
                    },
                    RawRef {
                        name: "mystery".into(),
                        line: 40,
                        kind: RK::UnknownIndirect,
                    },
                ],
            )
            .unwrap();
        let ans = DeadCodeService::assess(&store, 50).unwrap();
        let find = |name: &str, file: &str| {
            ans.data
                .iter()
                .find(|a| {
                    let n = match a {
                        DeadCodeAssessment::Candidate { node } => node,
                        DeadCodeAssessment::IndirectlyReferenced { node, .. } => node,
                        DeadCodeAssessment::Excluded { node, .. } => node,
                        DeadCodeAssessment::Unknown { node, .. } => node,
                    };
                    n.name == name && n.file == file
                })
                .unwrap_or_else(|| panic!("{file}:{name} missing from {:?}", ans.data))
        };
        assert!(matches!(
            find("cb_target", "a.rs"),
            DeadCodeAssessment::IndirectlyReferenced { kinds, .. } if kinds.contains(&RK::Callback)
        ));
        assert!(matches!(
            find("val_target", "a.rs"),
            DeadCodeAssessment::IndirectlyReferenced { kinds, .. } if kinds.contains(&RK::FunctionValue)
        ));
        assert!(
            matches!(find("pub_api", "a.rs"), DeadCodeAssessment::Unknown { .. }),
            "a pub fn with no in-repo references is unknowable, not a certain candidate"
        );
        assert!(
            matches!(
                find("pub_api", "b.rs"),
                DeadCodeAssessment::Candidate { .. }
            ),
            "the non-pub twin must NOT inherit the sibling's export surface"
        );
        assert!(matches!(
            find("mystery", "a.rs"),
            DeadCodeAssessment::Unknown { .. }
        ));
        assert!(matches!(
            find("truly_dead", "c.rs"),
            DeadCodeAssessment::Candidate { .. }
        ));
        assert!(matches!(
            find("main", "c.rs"),
            DeadCodeAssessment::Excluded { .. }
        ));
        assert!(matches!(
            find("handler_fn", "c.rs"),
            DeadCodeAssessment::IndirectlyReferenced { kinds, .. }
                if kinds.contains(&RK::FrameworkRegistration)
        ));
        // v1 candidates: only true candidates, in deterministic file/line order
        let cand = DeadCodeService::candidates(&store, 50).unwrap().data;
        let names: Vec<(String, String)> = cand
            .iter()
            .map(|c| (c.file.clone(), c.name.clone()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("b.rs".into(), "pub_api".into()),
                ("c.rs".into(), "truly_dead".into())
            ],
            "candidates are exactly the no-evidence symbols, file/line ordered"
        );
        // determinism: a second run returns the identical order
        let cand2 = DeadCodeService::candidates(&store, 50).unwrap().data;
        assert_eq!(
            cand.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            cand2.iter().map(|c| c.id.as_str()).collect::<Vec<_>>()
        );
        // LIMIT keeps the same prefix
        let first = DeadCodeService::candidates(&store, 1).unwrap().data;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id.as_str(), cand[0].id.as_str());
    }

    /// Acceptance 1+2: same-name symbols have INDEPENDENT fan-in and coverage
    /// at the service layer (ID-keyed, never name-keyed).
    #[test]
    fn same_name_rows_are_independent() {
        let store = Store::open_in_memory().unwrap();
        let mk = |id: &str, file: &str| codegraph_core::Node {
            id: id.into(),
            label: codegraph_core::NodeLabel::Function,
            name: "process".into(),
            file_path: file.into(),
            line_start: 1,
            line_end: 2,
            language: "python".into(),
            metadata: codegraph_core::Metadata::new(),
            community: None,
            pagerank: 0.0,
            betweenness: 0.0,
        };
        let a = mk("p.a.process", "a.py");
        let b = mk("p.b.process", "b.py");
        store.bulk_upsert_nodes(&[a.clone(), b.clone()]).unwrap();
        // one resolved Tests edge + one Calls edge to a's ID only
        let edge = |dst: &str, relation| codegraph_core::Edge {
            src: "p.t.test_process".into(),
            dst: dst.into(),
            relation,
            tier: codegraph_core::ResolutionTier::TreeSitter,
            confidence: codegraph_core::Confidence::Inferred,
            src_file: "test_a.py".into(),
            src_line: 3,
            metadata: codegraph_core::Metadata::new(),
        };
        store
            .bulk_upsert_edges(&[
                edge("p.a.process", codegraph_core::EdgeRelation::Calls),
                edge("p.a.process", codegraph_core::EdgeRelation::Tests),
            ])
            .unwrap();
        let ra = ReviewService::row_for(&store, &a).unwrap();
        let rb = ReviewService::row_for(&store, &b).unwrap();
        assert_eq!(ra.fan_in_resolved, 1);
        assert_eq!(rb.fan_in_resolved, 0, "twin must not inherit fan-in");
        assert_eq!(ra.tested, TestEvidence::Resolved);
        assert_ne!(
            rb.tested,
            TestEvidence::Resolved,
            "twin must not inherit coverage"
        );
    }
}
