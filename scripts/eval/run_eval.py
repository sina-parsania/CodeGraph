#!/usr/bin/env python3
"""Answer each ground-truth question with (a) codegraph `callers` and (b) a
grep baseline; score file-set precision/recall vs the SCIP ground truth and
count output bytes (the token-cost proxy an agent would pay).

Run gen_ground_truth.py first. Results: eval_results.json + a markdown summary
on stdout — the reproducible receipts behind the README's claims.
"""
import json
import pathlib
import subprocess
import tomllib

HERE = pathlib.Path(__file__).parent
WORK = HERE / "work"


def score(predicted: set[str], truth: set[str]):
    tp = len(predicted & truth)
    p = tp / len(predicted) if predicted else 0.0
    r = tp / len(truth) if truth else 0.0
    return p, r


def codegraph_files(repo_dir: pathlib.Path, name: str):
    # SQL keeps extraction robust (no CLI text parsing); byte cost still uses
    # the human `callers` output — that's what an agent actually reads.
    human = subprocess.run(
        ["codegraph", "callers", name, "--path", str(repo_dir)],
        capture_output=True, text=True,
    ).stdout
    sql = (
        "SELECT DISTINCT s.file_path FROM edges e "
        "JOIN nodes s ON s.id = e.src JOIN nodes d ON d.id = e.dst "
        f"WHERE e.relation = 'Calls' AND d.name = '{name}'"
    )
    rows = subprocess.run(
        ["codegraph", "query", sql, "--path", str(repo_dir), "--limit", "500"],
        capture_output=True, text=True,
    ).stdout
    files = {
        line.strip() for line in rows.splitlines()
        if "/" in line and not line.startswith(("file_path", "-"))
    }
    return files, len(human.encode())


def grep_files(repo_dir: pathlib.Path, name: str):
    out = subprocess.run(
        ["grep", "-rn", f"{name}(", "--include=*.py", "--include=*.ts", "--include=*.tsx", "."],
        capture_output=True, text=True, cwd=repo_dir,
    ).stdout
    files = {line.split(":", 1)[0].lstrip("./") for line in out.splitlines() if ":" in line}
    return files, len(out.encode())


def main():
    cfg = tomllib.loads((HERE / "repos.toml").read_text())
    results = []
    for repo in cfg["repo"]:
        qfile = HERE / "questions" / f"{repo['name']}.jsonl"
        if not qfile.exists():
            print(f"skip {repo['name']}: run gen_ground_truth.py first")
            continue
        repo_dir = WORK / repo["name"]
        subprocess.run(["codegraph", "index", "--path", str(repo_dir)], capture_output=True)
        for line in qfile.read_text().splitlines():
            q = json.loads(line)
            truth = set(q["files"])
            cg, cg_bytes = codegraph_files(repo_dir, q["name"])
            gr, gr_bytes = grep_files(repo_dir, q["name"])
            cg_p, cg_r = score(cg, truth)
            gr_p, gr_r = score(gr, truth)
            results.append({
                "repo": repo["name"], "name": q["name"],
                "codegraph": {"precision": cg_p, "recall": cg_r, "bytes": cg_bytes},
                "grep": {"precision": gr_p, "recall": gr_r, "bytes": gr_bytes},
            })
    (HERE / "eval_results.json").write_text(json.dumps(results, indent=2))
    if not results:
        return
    def avg(tool, key):
        return sum(r[tool][key] for r in results) / len(results)
    print(f"# eval — {len(results)} who-calls questions (SCIP ground truth)\n")
    print("| tool | precision | recall | avg bytes |")
    print("|---|---:|---:|---:|")
    for tool in ("codegraph", "grep"):
        print(f"| {tool} | {avg(tool,'precision'):.2f} | {avg(tool,'recall'):.2f} | {avg(tool,'bytes'):,.0f} |")


if __name__ == "__main__":
    main()
