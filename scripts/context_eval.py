#!/usr/bin/env python3
"""Context-selection eval: does CodeGraph's graph-aware `context` recover the files
a change actually touches better than pure name-match (aider repo-map's mechanism)?

Ground truth = the code files a git commit modified. Query = the commit subject.
For each commit we measure recall@budget of those files for:
  - CodeGraph `context` : personalized PageRank over the RESOLVED call graph
  - name-match baseline : `codegraph search` (FTS over symbol names) — the same
                          name-match signal aider's repo-map ranks on.
The thesis: `context` expands from query seeds through resolved CALL edges, so it
recovers relevant files that are call-graph-connected but NOT name-matched.

Run:  python3 scripts/context_eval.py [--repo .] [--commits 40] [--budget 1500]
Honest: this compares against aider's NAME-MATCH signal, not aider's full
personalized-PageRank+budget machinery. It isolates the resolved-graph advantage.
"""
import subprocess, os, sys, re, json

CG = os.environ.get("CODEGRAPH_BIN", "./target/release/codegraph")
REPO = os.path.abspath(next((a.split("=", 1)[1] if "=" in a else sys.argv[sys.argv.index(a) + 1]
                             for a in sys.argv if a.startswith("--repo")), "."))
NC = int(next((a.split("=", 1)[1] if "=" in a else sys.argv[sys.argv.index(a) + 1]
               for a in sys.argv if a.startswith("--commits")), "40"))
BUDGET = int(next((a.split("=", 1)[1] if "=" in a else sys.argv[sys.argv.index(a) + 1]
                   for a in sys.argv if a.startswith("--budget")), "1500"))
CODE_EXT = (".rs", ".ts", ".tsx", ".js", ".py", ".go", ".swift", ".kt", ".java", ".rb", ".c", ".cpp", ".cs")
STOP = {"the", "a", "an", "to", "of", "in", "for", "and", "fix", "add", "use", "with", "on", "feat",
        "chore", "refactor", "docs", "test", "wip", "update", "remove", "make", "via", "from", "into"}


def run(cmd):
    return subprocess.run(cmd, capture_output=True, text=True, cwd=REPO).stdout


def commits():
    out = run(["git", "log", "--no-merges", f"-{NC*2}", "--pretty=%H%x09%s"])
    res = []
    for line in out.splitlines():
        if "\t" not in line:
            continue
        h, subj = line.split("\t", 1)
        files = [f for f in run(["git", "show", "--name-only", "--pretty=", h]).splitlines()
                 if f.endswith(CODE_EXT) and os.path.isfile(os.path.join(REPO, f))]
        if 1 <= len(files) <= 8 and len(subj) > 8:  # focused, non-trivial commits
            res.append((subj, set(files)))
        if len(res) >= NC:
            break
    return res


def files_from(out):
    fs = []
    for line in out.splitlines():
        m = re.search(r'([\w./\-]+\.\w+):\d+', line)
        if m and m.group(1) not in fs:
            fs.append(m.group(1))
    return fs


def recall(predicted, truth, cap):
    pset = set(predicted[:cap])
    return len(pset & truth) / len(truth) if truth else 0.0


def main():
    run([CG, "index", REPO])
    data = commits()
    if not data:
        print("no suitable commits found")
        return
    cg_tot = base_tot = 0.0
    cg_wins = base_wins = ties = 0
    for subj, truth in data:
        cap = max(3, len(truth) * 2)  # same file budget for both methods
        cg_files = files_from(run([CG, "context", subj, "--path", REPO, "--budget", str(BUDGET), "--no-autoheal"]))
        # Fair name-match baseline: the SAME lenient OR-of-prefixes seeding `context`
        # uses, but only the direct FTS hits (no resolved-graph expansion). The gap
        # between this and `context` is exactly what the call graph recovers.
        toks = [w for w in re.findall(r"[A-Za-z][A-Za-z0-9]+", subj.lower()) if w not in STOP and len(w) > 2]
        fts = " OR ".join(f"{w}*" for w in toks) or subj
        base_files = files_from(run([CG, "search", fts, "--path", REPO, "--limit", str(cap * 3), "--no-autoheal"]))
        rc, rb = recall(cg_files, truth, cap), recall(base_files, truth, cap)
        cg_tot += rc
        base_tot += rb
        if rc > rb + 1e-9:
            cg_wins += 1
        elif rb > rc + 1e-9:
            base_wins += 1
        else:
            ties += 1
    n = len(data)
    print(f"repo: {REPO}   commits evaluated: {n}   budget: {BUDGET} tok")
    print(f"  CodeGraph context  (graph-aware) mean recall@files: {cg_tot/n*100:.1f}%")
    print(f"  name-match search  (aider signal) mean recall@files: {base_tot/n*100:.1f}%")
    print(f"  per-commit: context wins {cg_wins} · name-match wins {base_wins} · ties {ties}")
    lift = (cg_tot - base_tot) / base_tot * 100 if base_tot > 1e-9 else float("inf")
    print(f"  → context recovers {lift:+.0f}% more touched files than name-match" if lift!=float("inf") else "  → name-match recovered ~nothing; context is strictly better here.")
    if "--json" in sys.argv:
        print(json.dumps({"commits": n, "context_recall": round(cg_tot/n, 3),
                          "namematch_recall": round(base_tot/n, 3),
                          "context_wins": cg_wins, "namematch_wins": base_wins, "ties": ties}))


if __name__ == "__main__":
    main()
