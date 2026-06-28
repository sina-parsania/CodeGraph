#!/usr/bin/env python3
"""Reproducible token benchmark: answering code-navigation questions WITH CodeGraph
vs WITHOUT (a competent agent using ripgrep + bounded reads).

The metric is *context tokens ingested to reach the correct answer* — what an LLM
agent actually pays for.

BASELINE HONESTY (this is the whole point):
  A competent agent's grep cost is TASK-DEPENDENT, so we model it per task kind,
  the same way for this repo and for any external --repo:
    - "def"  (where is X defined?)  → grep the name, then read ONE definition
                                       region (a few candidate windows). NOT the
                                       whole file, NOT context around every usage.
    - "refs" (who calls X? where used?) → grep + read ±CTX lines around EVERY hit,
                                       because you must inspect each call site.
    - "body" (what does X call?)    → read X's own body (one region).
  The whole-file number is still computed and shown, but only as a labelled NAIVE
  UPPER BOUND. The headline ratios use the bounded, task-appropriate baseline.

Run from the repo root:  python3 scripts/benchmark.py
  --repo <path>      benchmark an external repo (tasks auto-discovered)
  --context N        context lines for "refs" tasks (default 5)

Token estimate = chars / 4 (the usual code rule of thumb); ratios are what matter.
"""
import subprocess, os, sys, json

CG = os.environ.get("CODEGRAPH_BIN", "./target/release/codegraph")
TOK = lambda s: max(1, len(s) // 4)
REPO = os.path.abspath(next((a.split("=", 1)[1] if "=" in a else sys.argv[sys.argv.index(a) + 1]
                             for a in sys.argv if a.startswith("--repo")), "."))
CTX = int(next((a.split("=", 1)[1] if "=" in a else sys.argv[sys.argv.index(a) + 1]
                for a in sys.argv if a.startswith("--context")), "5"))

_filecache = {}


def run(cmd):
    return subprocess.run(cmd, capture_output=True, text=True).stdout


def lines_of(f):
    if f not in _filecache:
        try:
            _filecache[f] = open(f, encoding="utf-8", errors="ignore").read().splitlines()
        except OSError:
            _filecache[f] = []
    return _filecache[f]


def rg_hits(pattern):
    """grep -n output + parsed [(file, line), ...] hits."""
    out = run(["rg", "-n", "--no-heading", pattern, REPO])
    hits = []
    for line in out.splitlines():
        parts = line.split(":", 2)
        if len(parts) >= 2 and os.path.isfile(parts[0]):
            try:
                hits.append((parts[0], int(parts[1])))
            except ValueError:
                pass
    return out, hits


def window_tokens(hits, radius, cap):
    """Tokens to read ±radius lines around up to `cap` hits (overlaps deduped per file)."""
    spans = {}
    for f, ln in (hits[:cap] if cap else hits):
        L = lines_of(f)
        for i in range(max(0, ln - 1 - radius), min(len(L), ln + radius)):
            spans.setdefault(f, set()).add(i)
    t = 0
    for f, idxs in spans.items():
        L = lines_of(f)
        t += TOK("\n".join(L[i] for i in sorted(idxs)))
    return t


def grep_cost(kind, pattern):
    """(grep_only, realistic_bounded, whole_file_upper_bound, n_hits) for a task kind."""
    plain, hits = rg_hits(pattern)
    go = TOK(plain)
    whole = go + sum(TOK("\n".join(lines_of(f))) for f in {f for f, _ in hits})
    if kind == "refs":                       # inspect context around every call site
        realistic = TOK(run(["rg", "-n", "--no-heading", "-C", str(CTX), pattern, REPO]))
    elif kind == "body":                     # read X's own body once
        realistic = go + window_tokens(hits, 30, 1)
    else:                                    # "def": scan hits, read a few candidate regions
        realistic = go + window_tokens(hits, 20, 3)
    return go, realistic, whole, len(hits)


def discover_tasks():
    """For an external --repo: per central symbol, a where-defined + a who-calls task."""
    run([CG, "index", REPO])
    out = run([CG, "important", "--path", REPO, "--limit", "6", "--no-autoheal"])
    tasks = []
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1].isidentifier() and len(parts[1]) > 2:
            n = parts[1]
            tasks.append((f"Where is `{n}` defined?", "def", n, ["search", n]))
            tasks.append((f"Who calls `{n}`?", "refs", n, ["callers", n]))
        if len(tasks) >= 6:
            break
    return tasks


# Self-repo defaults: fixed real symbols, each tagged with the honest task kind.
TASKS = [
    ("Where is `index_dir` defined?", "def", "index_dir", ["search", "index_dir"]),
    ("Who calls `ensure_fresh`?", "refs", "ensure_fresh", ["callers", "ensure_fresh"]),
    ("What does `run_init` call?", "body", "run_init", ["callees", "run_init"]),
    ("Where is `OpenAiCompatBackend` used?", "refs", "OpenAiCompatBackend", ["search", "OpenAiCompatBackend"]),
    ("Who calls `db_path`?", "refs", "db_path", ["callers", "db_path"]),
    ("Where is `Store` defined?", "def", "Store", ["search", "Store"]),
] if REPO == os.path.abspath(".") else discover_tasks()

rows = []
tot_only = tot_real = tot_whole = tot_cg = tot_calls_grep = tot_calls_cg = 0
for q, kind, pat, cg in TASKS:
    go, real, whole, nh = grep_cost(kind, pat)
    cg_tok = TOK(run([CG, *cg, "--path", REPO, "--no-autoheal"]))
    rows.append((q, kind, go, real, whole, cg_tok))
    tot_only += go
    tot_real += real
    tot_whole += whole
    tot_cg += cg_tok
    tot_calls_grep += 1 + nh
    tot_calls_cg += 1

w = max(len(r[0]) for r in rows)
print(f"repo: {REPO}   refs context: ±{CTX} lines")
print(f"{'task'.ljust(w)}  kind   grep-only  grep+bounded  grep+whole  codegraph")
print("-" * (w + 54))
for q, kind, go, real, whole, cg in rows:
    print(f"{q.ljust(w)}  {kind:<5}  {go:>9}  {real:>12}  {whole:>10}  {cg:>9}")
print("-" * (w + 54))
print(f"{'TOTAL tokens'.ljust(w)}  {'':<5}  {tot_only:>9}  {tot_real:>12}  {tot_whole:>10}  {tot_cg:>9}")

ratio_real = tot_real / tot_cg if tot_cg else 0
ratio_whole = tot_whole / tot_cg if tot_cg else 0
saving = 100 * (1 - tot_cg / tot_real) if tot_real else 0
print(f"\nHEADLINE (realistic, bounded per-task baseline): CodeGraph {tot_cg} tokens "
      f"vs {tot_real} → {ratio_real:.0f}× fewer context tokens ({saving:.1f}%).")
print(f"Naive upper bound (grep + read whole hit files): {tot_whole} → {ratio_whole:.0f}× — "
      "NOT the headline; a real agent reads bounded regions, not whole files.")
print(f"Tool round-trips: {tot_calls_grep} (grep + reads) → {tot_calls_cg} (codegraph).")
print("\nGraph-only queries grep CANNOT answer without reading much of the tree: "
      "impact/blast-radius, trace (shortest path), important (PageRank), communities.")

if "--json" in sys.argv:
    print(json.dumps({"repo": REPO, "context_lines": CTX, "grep_realistic": tot_real,
                      "grep_whole": tot_whole, "codegraph": tot_cg,
                      "ratio_realistic": round(ratio_real), "ratio_whole": round(ratio_whole),
                      "calls": [tot_calls_grep, tot_calls_cg]}))
