# Reproducible eval — who-calls accuracy + token cost vs SCIP ground truth

Not marketing numbers: pinned OSS repos, compiler-derived ground truth, one
command to reproduce. This is the receipt behind the README's claims.

## Requirements
- `git`, `python3.11+`
- the `scip` CLI (https://github.com/sourcegraph/scip/releases)
- per-repo indexers from `repos.toml` (e.g. `npm i -g @sourcegraph/scip-typescript @sourcegraph/scip-python`)
- `codegraph` on PATH (release build)

## Run
```bash
python3 scripts/eval/gen_ground_truth.py   # clone pins, SCIP-index, derive questions
python3 scripts/eval/run_eval.py           # answer with codegraph + grep, score, print table
```

## Method
- **Questions**: function symbols (SCIP `().` suffix) referenced by 2–30 files;
  ground truth = the file set the compiler saw referencing them.
- **Scoring**: file-set precision/recall per question, averaged; plus raw output
  bytes as the token-cost proxy an agent pays to read the answer.
- **Baselines**: `grep -rn "name("` — the tool an agent uses without a graph.

Deterministic: pinned SHAs, seeded sampling (SEED=0). Add repos to
`repos.toml` to broaden the corpus. Not wired into CI (network + indexers).
