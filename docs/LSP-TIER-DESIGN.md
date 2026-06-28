# Design note — an optional LSP-backed resolution tier (T4 → **LSP** → SCIP)

> **Status: DESIGN ONLY — not implemented. Awaiting go/no-go.**
> Addresses the review's Issue 3: "compiler-grade precision" via SCIP needs an
> installed indexer + a working build, so most users only ever see the
> tree-sitter core (~25% of the addressable call bucket). An LSP is often already
> installed where a SCIP indexer is not — can it be a precise middle tier?

## TL;DR

Yes, but **narrowly and Swift-first**, as an **opt-in, off-by-default** tier exactly
like SCIP. The justification is almost entirely **Swift**: `scip-swift` does not
exist, so Swift has _no_ compiler-grade option today, yet `sourcekit-lsp` + its
`IndexStoreDB` ship free with every Xcode/CLT install and the index store is
**usually already populated** in DerivedData from the developer's normal builds.
For every other language a SCIP indexer already exists and is the better escalation,
so a general per-language LSP fleet is **not worth** the determinism, warmup, and
packaging cost.

## Feasibility — three bands

All candidate servers speak `textDocument/definition` + `textDocument/references`
headlessly over stdio JSON-RPC. The dividing line is **how much build model the
server needs before its cross-file answers are correct**:

| Band                               | Servers                                                                                                                                        | Build needed?                                               | Verdict                                                             |
| ---------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- | ------------------------------------------------------------------- |
| **A — source-resolving, no build** | pyright (needs `diagnosticMode:workspace`), typescript-language-server (lazy per-file; must `didOpen` the tree), pylsp                         | none                                                        | works, but these langs already have scip-python/scip-typescript     |
| **B — needs build/classpath**      | jdtls (Maven/Gradle), kotlin-language-server (Gradle classpath), rust-analyzer (cargo metadata, ~5 s/100 crates, up to ~1 min), gopls (go.mod) | yes                                                         | burden ≈ the SCIP indexer we already merge → **low marginal value** |
| **C — Swift (the crux)**           | sourcekit-lsp / **IndexStoreDB**                                                                                                               | reads the compiler's index store; **usually already built** | **the one real gap LSP fills**                                      |

### The Swift insight (what the recommendation pivots on)

`sourcekit-lsp` serves compiler-accurate cross-file references from a **global index
store** (`IndexStoreDB`) that `swiftc`/Xcode populate **during normal builds** (indexing-
while-building, in DerivedData). For a developer who builds in Xcode daily, that index
**already exists** — so CodeGraph can read compiler-grade Swift references **without
triggering any build of its own**.

Better still, you don't need the LSP server at all: Apple's
[`swiftlang/indexstore-db`](https://github.com/swiftlang/indexstore-db) (over `libIndexStore`)
reads the store directly and returns `SymbolOccurrence`s (file/line/role) — exactly
find-references, **no JSON-RPC, no warmup, no nondeterministic server**. Proven by
[SwiftFindRefs](https://github.com/michaelversus/SwiftFindRefs) and
[swift-index-store](https://github.com/MobileNativeFoundation/swift-index-store). A small
Rust FFI/subprocess reader gives compiler-accurate Swift edges and degrades cleanly to
"no store → drop, fall back to T0–T4."

## How it slots into T0–T4

Add `ResolutionTier::Lsp`, precedence **`Scip > Lsp > TreeSitter`**. It is a precise
tier **above T4** (fires only on calls T0–T4 dropped/left unlinked) and **below SCIP**
(a present `.scip` always wins). Mechanism mirrors `import_scip` exactly:

1. For each unresolved call site, issue `textDocument/definition` **at the call
   position** (definition is the natural inverse of "which def does this call hit" —
   one round-trip per site, no reverse mapping).
2. Map the returned `Location` to an internal node by file+line, same as
   `import_scip` maps occurrences.
3. Emit a CALLS edge **only when it maps to exactly ONE internal node, else DROP** —
   this preserves the sacred unique-or-drop / zero-phantom invariant. An external
   target (stdlib/dep, no internal node) simply drops, as today.
4. Tag `tier = Lsp`, dedup on `(src, dst, relation)`, merge at `build()` after
   tree-sitter so a SCIP edge for the same pair supersedes.

## Recall it could recover

When the server is **warm and the project resolves on this machine**, an LSP tier
approaches the SCIP/compiler ceiling — it resolves the two classes T0–T4 deliberately
drop: (a) **ambiguous local-variable receivers** (`let x = Foo(); x.method()`) CHA can't
type, and (b) **cross-file/cross-module** calls where the name is globally non-unique or
routed through re-exports/aliases. Against `RESOLUTION.md`'s addressable core (TS
10.2%→52.3% after T1+T3; Swift "meaningfully above 25.5%"), a compiler tier lifts the
resolvable share toward the SCIP ceiling. **Conditional, not flat:** cold/unresolvable
project → 0 gain; warm/indexed → near-SCIP. On Swift specifically, recovering most of the
residual ambiguous-receiver + cross-file bucket = a double-digit-point gain on top of T1–T4.

## Costs & caveats (why it must be gated)

- **Determinism** — CodeGraph guarantees byte-identical graphs (same commit → same DB).
  An external LSP makes edges depend on **this machine's** toolchain version, resolved
  deps, and index-store state → two developers can get different graphs. Must be
  opt-in/off-by-default like SCIP; never touch the default deterministic path.
- **Single-binary distribution is lost** the moment you spawn language servers → a
  **sidecar**, not in-process. The Swift IndexStore-direct reader is the one exception
  that stays lean (link `libIndexStore` / shell to a small reader).
- **Warmup** — rust-analyzer up to ~1 min on large dep graphs; jdtls/gradle slow.
- **Phantom-edge safety preserved ONLY by keeping unique-or-drop** — never widen LSP
  results to "best guess."
- **Swift Xcode gap** — background indexing is SwiftPM-modeled; a pure-Xcode repo with
  no recent build may have no store (mitigated by the common case: DerivedData already
  populated).

## Recommendation

1. **Build the Swift path first via the IndexStore reader** (not a running server) —
   the only place LSP clears a bar SCIP cannot. Gate behind `--lsp` / `--indexstore`,
   off by default. Degrades cleanly to T0–T4.
2. **Do NOT prioritize Band B** (Java/Kotlin/Rust/Go) — same build burden as SCIP, which
   already wins there.
3. **Band A** (pyright / tsserver) is a defensible _later, opportunistic_ add (no build),
   but those langs already have SCIP indexers and a smaller gap.
4. Keep unique-or-drop; merge/dedup like `import_scip`; isolate any real LSP servers in a
   sidecar if ever added.

**One-line verdict:** a thin opt-in compiler-grade tier between T4 and SCIP, with the
effort spent on **Swift via the IndexStore** (the unique gap); for everything else SCIP
already wins and an LSP fleet isn't worth the determinism/warmup/packaging cost.

## Sources

sourcekit-lsp background indexing & IndexStoreDB; SwiftFindRefs; swift-index-store;
rust-analyzer config/startup; gopls workspace; typescript-language-server config;
basedpyright import resolution; eclipse jdt.ls; fwcd/kotlin-language-server. (Full URLs
in the review thread that produced this note.)
