//! `codegraph audit` — MEASURED precision, not claimed. Samples tree-sitter
//! CALLS edges and verifies each against a compiler-grade oracle already merged
//! into this graph (SCIP / Xcode IndexStore). Per-justification-tier precision
//! is stored in meta (`audit_result`) and surfaced in MCP `stats` and the
//! report, so an agent knows exactly how much to trust each evidence class ON
//! THIS REPO — the honest replacement for made-up numeric confidence weights.
//!
//! Method: an edge is VERIFIABLE iff the oracle saw any outgoing call from its
//! source symbol (the compiler indexed that function); it is CONFIRMED iff the
//! oracle contains the same (src, dst). Sources outside oracle coverage are
//! counted `unverifiable` and excluded from precision. Deterministic: sorted
//! edges + seeded xorshift sampling; results live in meta only (never hashed).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use codegraph_core::{EdgeRelation, ResolutionTier};
use codegraph_store::Store;

use crate::index;

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0.max(1);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

pub fn run(root: &Path, sample: usize, seed: u64, json_out: bool) -> Result<()> {
    let store = Store::open(&index::db_path(root))?;
    let edges = store.all_edges()?;

    // Oracle = every compiler-grade CALLS edge already merged into this graph.
    let oracle: HashSet<(&str, &str)> = edges
        .iter()
        .filter(|e| e.tier == ResolutionTier::Scip && e.relation == EdgeRelation::Calls)
        .map(|e| (e.src.as_str(), e.dst.as_str()))
        .collect();
    let covered_srcs: HashSet<&str> = oracle.iter().map(|(s, _)| *s).collect();
    if oracle.is_empty() {
        anyhow::bail!(
            "no compiler-grade oracle in this graph — run `codegraph scip` (or build the Xcode \
             index on macOS) first, then re-run `codegraph audit`"
        );
    }
    // Candidates: tree-sitter's OWN resolution, rebuilt in memory from the raw
    // tables. The persisted graph is post-merge — every tree-sitter edge that
    // AGREED with the oracle was superseded by it, so sampling persisted
    // TreeSitter edges would (wrongly) measure only the disagreements.
    let nodes = store.graph_nodes()?;
    // Which languages the oracle actually covers — precision is only measured
    // THERE; tiers whose language the oracle never saw stay `unverifiable`.
    let lang_of: HashMap<&str, &str> = nodes
        .iter()
        .map(|n| (n.id.as_str(), n.language.as_str()))
        .collect();
    let mut oracle_langs: Vec<&str> = covered_srcs
        .iter()
        .filter_map(|s| lang_of.get(s).copied())
        .collect();
    oracle_langs.sort_unstable();
    oracle_langs.dedup();
    let oracle_langs: Vec<String> = oracle_langs.into_iter().map(String::from).collect();
    let built = codegraph_graph::build_with(
        &nodes,
        &store.all_calls()?,
        &store.all_inherits()?,
        &store.all_fields()?,
        &store.all_locals()?,
        &store.all_imports()?,
        false,
    );
    let mut candidates: Vec<(String, String, String)> = built
        .edges
        .iter()
        .filter(|e| e.tier == ResolutionTier::TreeSitter && e.relation == EdgeRelation::Calls)
        .map(|e| {
            let j = e
                .metadata
                .get("justification")
                .and_then(|v| v.as_str())
                .unwrap_or("untagged")
                .to_string();
            (e.src.clone(), e.dst.clone(), j)
        })
        .collect();
    candidates.sort();

    // Seeded sample without replacement.
    let mut rng = XorShift(seed.wrapping_add(0x9E3779B97F4A7C15));
    let take = sample.min(candidates.len());
    let mut picked: Vec<usize> = Vec::with_capacity(take);
    let mut chosen = HashSet::new();
    while picked.len() < take {
        let i = (rng.next() % candidates.len() as u64) as usize;
        if chosen.insert(i) {
            picked.push(i);
        }
    }
    picked.sort();

    #[derive(Default)]
    struct Tally {
        checked: usize,
        confirmed: usize,
        unverifiable: usize,
    }
    let mut tiers: BTreeMap<String, Tally> = BTreeMap::new();
    for &i in &picked {
        let (ref src, ref dst, ref j) = candidates[i];
        let t = tiers.entry(j.clone()).or_default();
        if covered_srcs.contains(src.as_str()) {
            t.checked += 1;
            if oracle.contains(&(src.as_str(), dst.as_str())) {
                t.confirmed += 1;
            }
        } else {
            t.unverifiable += 1;
        }
    }

    let tier_json: serde_json::Map<String, serde_json::Value> = tiers
        .iter()
        .map(|(j, t)| {
            let precision =
                if t.checked > 0 { t.confirmed as f64 / t.checked as f64 } else { f64::NAN };
            (
                j.clone(),
                serde_json::json!({
                    "checked": t.checked, "confirmed": t.confirmed,
                    "unverifiable": t.unverifiable,
                    "precision": if t.checked > 0 { serde_json::json!((precision * 1000.0).round() / 1000.0) } else { serde_json::Value::Null },
                }),
            )
        })
        .collect();
    let (checked, confirmed): (usize, usize) = tiers
        .values()
        .fold((0, 0), |(c, k), t| (c + t.checked, k + t.confirmed));
    let result = serde_json::json!({
        "oracle_edges": oracle.len(),
        "oracle_languages": oracle_langs,
        "sampled": take,
        "seed": seed,
        "tiers": tier_json,
        "overall": {
            "checked": checked, "confirmed": confirmed,
            "precision": if checked > 0 { serde_json::json!(((confirmed as f64 / checked as f64) * 1000.0).round() / 1000.0) } else { serde_json::Value::Null },
        },
        "note": "precision is a LOWER BOUND on oracle-covered code only: an oracle disagreement is counted as unconfirmed even when the compiler bound a different-but-related target (protocol requirement vs implementation); tiers with checked=0 are NOT verified by this oracle",
        "generation": codegraph_store::generation(&index::db_path(root)),
    });
    store.meta_set("audit_result", &result.to_string())?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!(
        "# measured precision vs compiler oracle ({} oracle edges, langs: {}, {} sampled, seed {seed})",
        oracle.len(),
        if oracle_langs.is_empty() { "?".to_string() } else { oracle_langs.join("+") },
        take
    );
    println!("# LOWER BOUND on oracle-covered code; tiers with checked=0 are not verified by this oracle\n");
    println!(
        "{:<20} {:>8} {:>10} {:>13} {:>10}",
        "tier", "checked", "confirmed", "unverifiable", "precision"
    );
    for (j, t) in &tiers {
        let p = if t.checked > 0 {
            format!("{:.1}%", t.confirmed as f64 * 100.0 / t.checked as f64)
        } else {
            "—".into()
        };
        println!(
            "{j:<20} {:>8} {:>10} {:>13} {:>10}",
            t.checked, t.confirmed, t.unverifiable, p
        );
    }
    if checked > 0 {
        println!("\noverall: {confirmed}/{checked} = {:.1}%  (stored in meta `audit_result`; shown in MCP stats + report)", confirmed as f64 * 100.0 / checked as f64);
    } else {
        println!("\nno sampled edge had oracle coverage — the oracle and tree-sitter tiers may cover disjoint files");
    }
    Ok(())
}
