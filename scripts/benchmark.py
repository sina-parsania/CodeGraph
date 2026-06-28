#!/usr/bin/env python3
"""Reproducible token benchmark: answering code-navigation questions WITH CodeGraph
vs WITHOUT (grep + reading files into the agent's context).

The metric is *context tokens ingested to reach the correct answer* — what an LLM
agent actually pays for. Without CodeGraph an agent greps, then must READ the
matched files to disambiguate real call sites from comments/strings/defs; with
CodeGraph it gets one compact, resolved `file:line` result.

Run from the repo root:  python3 scripts/benchmark.py
Token estimate = chars / 4 (the usual code rule of thumb); ratios are what matter.
"""
import subprocess, os, sys, json

CG = os.environ.get("CODEGRAPH_BIN", "./target/release/codegraph")
TOK = lambda s: max(1, len(s) // 4)
REPO = os.path.abspath(next((a.split("=", 1)[1] if "=" in a else sys.argv[sys.argv.index(a) + 1]
                             for a in sys.argv if a.startswith("--repo")), "."))


def run(cmd):
    return subprocess.run(cmd, capture_output=True, text=True).stdout


def rg_files(pattern):
    """Files an agent would open after grepping for `pattern` (unique hit files)."""
    out = run(["rg", "-n", "--no-heading", pattern, REPO])
    files = []
    for line in out.splitlines():
        p = line.split(":", 1)[0]
        if p and p not in files and os.path.isfile(p):
            files.append(p)
    return out, files


def read_tokens(files):
    t = 0
    for f in files:
        try:
            t += TOK(open(f, encoding="utf-8", errors="ignore").read())
        except OSError:
            pass
    return t


def discover_tasks():
    """For an external --repo, auto-pick real symbols (most central by PageRank)."""
    run([CG, "index", REPO])
    out = run([CG, "important", "--path", REPO, "--limit", "8", "--no-autoheal"])
    tasks = []
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1].isidentifier():
            n = parts[1]
            tasks.append((f"Where is `{n}`?", n, ["search", n]))
    return tasks[:6]


# Default: fixed real symbols in CodeGraph's own repo (stable, reproducible numbers).
# With `--repo <path>`: auto-discovered from that repo, so anyone can verify on their code.
TASKS = [
    ("Where is `index_dir` defined?", "fn index_dir", ["search", "index_dir"]),
    ("Who calls `ensure_fresh`?", "ensure_fresh", ["callers", "ensure_fresh"]),
    ("What does `run_init` call?", "fn run_init", ["callees", "run_init"]),
    ("Where is `OpenAiCompatBackend` used?", "OpenAiCompatBackend", ["search", "OpenAiCompatBackend"]),
    ("Who calls `db_path`?", "db_path", ["callers", "db_path"]),
    ("Where is `Store` defined?", "struct Store", ["search", "Store"]),
] if REPO == os.path.abspath(".") else discover_tasks()

rows, tot_grep_only, tot_grep_read, tot_cg, tot_calls_grep, tot_calls_cg = [], 0, 0, 0, 0, 0
for q, pat, cg in TASKS:
    rg_out, files = rg_files(pat)
    grep_only = TOK(rg_out)                     # grep output alone (often ambiguous → wrong)
    grep_read = grep_only + read_tokens(files)  # realistic: grep + read hit files (correct answer)
    cg_out = run([CG, *cg, "--path", REPO, "--no-autoheal"])
    cg_tok = TOK(cg_out)
    grep_calls = 1 + len(files)                 # 1 grep + N reads
    rows.append((q, grep_only, grep_read, cg_tok, grep_calls, len(files)))
    tot_grep_only += grep_only
    tot_grep_read += grep_read
    tot_cg += cg_tok
    tot_calls_grep += grep_calls
    tot_calls_cg += 1

w = max(len(r[0]) for r in rows)
print(f"{'task'.ljust(w)}  grep-only  grep+read   codegraph   tool-calls(grep→cg)")
print("-" * (w + 52))
for q, go, gr, cg, gc, nf in rows:
    print(f"{q.ljust(w)}  {go:>9}  {gr:>9}  {cg:>9}   {gc}→1")
print("-" * (w + 52))
print(f"{'TOTAL tokens'.ljust(w)}  {tot_grep_only:>9}  {tot_grep_read:>9}  {tot_cg:>9}   {tot_calls_grep}→{tot_calls_cg}")
saving = 100 * (1 - tot_cg / tot_grep_read) if tot_grep_read else 0
ratio = tot_grep_read / tot_cg if tot_cg else 0
print(f"\nCodeGraph: {tot_cg} tokens vs {tot_grep_read} for grep+read "
      f"→ {ratio:.0f}× fewer context tokens ({saving:.1f}%), {tot_calls_grep}→{tot_calls_cg} tool round-trips.")
print(f"Even grep-ONLY ({tot_grep_only} tokens) is {tot_grep_only/tot_cg:.0f}× more than CodeGraph — "
      "and returns ambiguous hits (defs/comments/strings), so the agent must read files anyway.")
print("\nGraph-only queries grep CANNOT answer without reading much of the tree: "
      "impact/blast-radius, trace (shortest path), important (PageRank), communities.")

if "--json" in sys.argv:
    print(json.dumps({"grep_read": tot_grep_read, "codegraph": tot_cg,
                      "saving_pct": round(saving), "calls": [tot_calls_grep, tot_calls_cg]}))
