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
    # `--files` is the tool's file-level answer: resolved caller files + the
    # `~`-prefixed parser-verified unresolved call-site files. Same granularity
    # as file-level rivals — bytes measured on exactly what an agent reads.
    out = subprocess.run(
        ["codegraph", "callers", name, "--path", str(repo_dir), "--files"],
        capture_output=True, text=True,
    ).stdout
    files = {line.strip().lstrip("~") for line in out.splitlines() if line.strip()}
    return files, len(out.encode())


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
                "codegraph": {"precision": cg_p, "recall": cg_r, "bytes": cg_bytes, "answered": bool(cg)},
                "grep": {"precision": gr_p, "recall": gr_r, "bytes": gr_bytes, "answered": bool(gr)},
            })
    (HERE / "eval_results.json").write_text(json.dumps(results, indent=2))
    if not results:
        return

    # Precision is averaged over ANSWERED questions only — an empty answer is a
    # refusal (codegraph drops unresolvable-by-evidence calls by design), and
    # counting it as precision=0 would punish correct refusal as imprecision.
    # The refusal rate is reported separately as answer-rate.
    def row(tool):
        answered = [r for r in results if r[tool]["answered"]]
        p = sum(r[tool]["precision"] for r in answered) / max(len(answered), 1)
        rc = sum(r[tool]["recall"] for r in results) / len(results)
        b = sum(r[tool]["bytes"] for r in results) / len(results)
        return p, rc, len(answered) / len(results), b

    print(f"# eval — {len(results)} who-references questions (SCIP ground truth)\n")
    print("| tool | precision (answered) | recall | answer rate | avg bytes |")
    print("|---|---:|---:|---:|---:|")
    for tool in ("codegraph", "grep"):
        p, rc, ar, b = row(tool)
        print(f"| {tool} | {p:.2f} | {rc:.2f} | {ar:.0%} | {b:,.0f} |")


if __name__ == "__main__":
    main()
