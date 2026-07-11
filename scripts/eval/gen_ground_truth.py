#!/usr/bin/env python3
"""Ground truth from SCIP: for each repo in repos.toml, clone at the pin, run
its SCIP indexer, and derive who-references questions from the index.

Requires: git, the per-repo indexer, and the `scip` CLI (`scip print --json`,
https://github.com/sourcegraph/scip/releases) on PATH.

Output: questions/<repo>.jsonl — one {"symbol", "name", "files"} per line where
`files` is the ground-truth set of files referencing that function symbol.
Function symbols are recognized by the SCIP suffix `().`.
"""
import json
import pathlib
import random
import subprocess
import sys
import tomllib

HERE = pathlib.Path(__file__).parent
WORK = HERE / "work"
QUESTIONS = HERE / "questions"
PER_REPO = 40  # questions sampled per repo
SEED = 0


def sh(cmd, cwd=None):
    print("+", " ".join(map(str, cmd)), file=sys.stderr)
    subprocess.run(cmd, cwd=cwd, check=True)


def main():
    cfg = tomllib.loads((HERE / "repos.toml").read_text())
    QUESTIONS.mkdir(exist_ok=True)
    for repo in cfg["repo"]:
        dest = WORK / repo["name"]
        if not dest.exists():
            sh(["git", "clone", "--depth", "1", "--branch", repo["sha"], repo["url"], str(dest)])
        scip_file = dest / "index.scip"
        if not scip_file.exists():
            sh([repo["indexer"], "index"], cwd=dest)
        dump = subprocess.run(
            ["scip", "print", "--json", str(scip_file)], capture_output=True, text=True, check=True
        ).stdout
        index = json.loads(dump)
        refs: dict[str, set[str]] = {}
        def_count_by_name: dict[str, int] = {}

        def bare(sym: str) -> str:
            return sym.rsplit("/", 1)[-1].removesuffix("().").split("#")[-1].split(".")[-1]

        for doc in index.get("documents", []):
            path = doc["relativePath"]
            for occ in doc.get("occurrences", []):
                sym = occ.get("symbol", "")
                if not sym.endswith(")."):
                    continue  # functions/methods only
                if occ.get("symbolRoles", 0) & 1:
                    def_count_by_name[bare(sym)] = def_count_by_name.get(bare(sym), 0) + 1
                else:
                    refs.setdefault(sym, set()).add(path)
        rng = random.Random(SEED)
        # answerable, non-trivial questions: 2..30 referencing files, and the
        # bare name must be UNIQUELY defined in the repo — otherwise the
        # name-keyed question is ambiguous and no tool can be scored fairly
        pool = sorted(
            (s, sorted(f))
            for s, f in refs.items()
            if 2 <= len(f) <= 30 and def_count_by_name.get(bare(s), 0) == 1
        )
        rng.shuffle(pool)
        out = QUESTIONS / f"{repo['name']}.jsonl"
        with out.open("w") as fh:
            for sym, files in pool[:PER_REPO]:
                name = bare(sym)
                if not name.isidentifier():
                    continue
                fh.write(json.dumps({"symbol": sym, "name": name, "files": files}) + "\n")
        print(f"{repo['name']}: {out} written", file=sys.stderr)


if __name__ == "__main__":
    main()
