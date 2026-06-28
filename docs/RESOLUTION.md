# Call resolution — tiered Class Hierarchy Analysis (CHA)

How CodeGraph turns a `RawCall` into a `CALLS` edge **without a compiler**, raising recall while
keeping **precision sacred** (no phantom edges). Chosen over GitHub stack-graphs after a 5-agent
study: stack-graphs 0.10 pins tree-sitter ^0.24 vs our 0.26.9 (ABI conflict), only 4/13 languages
have official `.tsg` packs (Swift has none), and it's a set/navigation resolver onto which we'd still
bolt the same uniqueness filter. One auditable `resolve_member()` beats a per-language scope DSL.

## The one precision surface

```
resolve_member(root_class, method_name) -> Option<node_id>:
    candidates = methods named `method_name` on root_class
                 + transitive INHERITS/IMPLEMENTS ancestors  (nearest-class-wins)
    return Some(id) iff the nearest level has exactly ONE candidate, else None  // DROP, never guess
```

Every new edge is `resolve_member` with a different _root_. The receiver type is read as a literal CST
token, never inferred.

## Resolution order (first match wins; each step provably correct)

| Tier   | Shape                                       | Root                                                                                                      | Guarantee                                                                                                   |
| ------ | ------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| **T0** | bare `foo()` same-file                      | caller's file                                                                                             | keep iff **exactly one** same-file def (tighten the current last-write-wins overwrite — precision-positive) |
| **T1** | `self.foo()` / `this.foo()` / `super.foo()` | enclosing class (super→parent)                                                                            | receiver type _is_ the class, statically                                                                    |
| **T2** | `Type.foo()` static                         | the named type, iff it resolves to exactly one node                                                       | uniqueness check                                                                                            |
| **T3** | `this.field.foo()` (the NestJS DI majority) | the field's declared type (from ctor parameter-properties / typed prop decls — literal `type_identifier`) | uniqueness check                                                                                            |
| **T4** | bare `foo()` global                         | existing `fn_by_name.len()==1`                                                                            | unchanged (no regression)                                                                                   |

Anything a tier can't resolve uniquely **drops** to T4 or is left unlinked — exactly as today.

## Drop conditions (enumerated — all DROP, none guess)

- ambiguous self (≥2 same-level members) · 0 matches · type-name collision · method not unique on type
- `self`/`this` inside a non-arrow JS/TS function literal that **rebinds `this`** (arrow fns / Swift closures / Python nested defs keep it bound — OK there)
- Python: only treat the receiver as self when it equals the method's **first parameter** name; `cls` (classmethod) is out of scope
- `super` only when the INHERITS chain resolved · `ClassName.foo()` static is **not** self/this · overloads (same name, ≥2 arity) drop (no arity/type info)

## Parser/data changes

- `RawCall` gains `receiver_kind: { Bare, SelfThis, Super, Named(String), FieldChain(String) }` + `enclosing_class: Option<String>` (+ `receiver_field` for FieldChain). Classify the immediate child under the callee field instead of flattening it in `trailing_ident`/`callee_name`.
- thread a `current_class` in `collect()` symmetric to the existing `current_fn`.
- per-language receiver detectors: TS/Java `this`/`super` token kind; Swift `self_expression`/`super_expression`; Python first-child identifier == first param name.
- **B1** (foundational bug): class-qualify Method node ids (`project.<segs>.<Class>.<name>`) so `class A{foo}` and `class B{foo}` in one file stop colliding to one id. Until B1, a containment-derived `class_members` map (class span ⊃ method span) covers the common case (class in its own file); the same-file-two-classes collision is a known, rarer limitation.
- **B2**: emit `MemberOf(method→class)` (the `EdgeRelation::MemberOf` already exists, unused).
- **B3**: Swift `extension Foo` (parses as `class_declaration` + `extension` token, no name field) → map to type `Foo` (1029 extension blocks mis-attributed in a real iOS corpus today).
- store: `receiver_kind`/`receiver_payload`/`enclosing_class` columns on `calls`; per-file field→type table mirroring `save_calls`; bump `SCHEMA_VERSION`.

## Resolver changes

- `build()` becomes tiered T0→T4, first match wins; each new tier emits **only** where the call is otherwise dropped.
- one `resolve_member()` helper; `class_members` from MemberOf (or containment pre-B1); `field_types` from the field→type table; both built from the global persisted sets inside `build()`.
- tag every CALLS edge with `Edge.metadata.justification ∈ {SameFileUnique, SelfThisMember, StaticTypeMember, FieldTypeMember, GlobalUnique}` + per-tier counters for measurement.
- global rebuild contract unchanged → determinism + incremental==full hold by construction.

## Precision proof obligations (tests)

1. **Justification invariant** — no `CALLS` edge without a justification tag.
2. **Determinism** — `build()` on identical input is byte-identical (extend `analyze_is_deterministic` to edges).
3. **Negative tests, one per drop rule** — ambiguous self → no edge; named-variable receiver mis-tagged → no edge; same-file duplicate name → no edge; duplicate type name (T2/T3) → no edge; method not unique on type → no edge; `self`-override → resolves to C's own member, not the parent's.

## Rollout

- **Phase 0** — B1 + B2 + B3 + justification tag; tighten T0 to `count==1`; bump schema. No new edges beyond the T0 correction.
- **Phase 1** — TS/TSX **T1** (self/this). Proof corpus: a NestJS backend (has baseline recall). Measure per-tier counters before claiming a delta.
- **Phase 2** — TS **T2** + **T3** (the DI lever — `this.field.method()` is the measured majority: 4794 vs 3058 same-class in backend-app).
- **Phase 3** — Swift T1+T2+T3 (needs B3 first).
- **Phase 4** — Java, Kotlin, C#, Python.
- **Phase 5** — Go, Rust (receiver-typed methods). C/C++/Bash stay on T0/T4. Optional: a bare-call import table (ES-module/Python/Go/Rust) as a precision-safe tier between T3 and T4.

## Measured — language-agnostic receiver resolution

Receiver detection is **language-agnostic** (the receiver is read from the callee's text — `self`/`this`/
`self.field`/named — so the same tiers fire for every grammar). Two real corpora, full re-index, isolated
caches, same metric (`… FROM edges WHERE relation='Calls'`):

| Corpus               | resolved CALLS edges (before → after) | what carried it                                            |
| -------------------- | ------------------------------------- | ---------------------------------------------------------- |
| NestJS backend (TS)  | 8,751 → **10,667**                    | T1 self/this + T3 DI fields (`this.service.method()`)      |
| iOS app (Swift)      | 9,711 → **18,244** (+88%)             | a dropped-call parse fix + T1 self/this + T4 global-unique |
| Android app (Kotlin) | 3,199 → **4,112**                     | same parse fix + T1 this                                   |

Every new edge is **provably correct**: T1 (`self.m()` → the enclosing class's `m`, unique-or-drop),
T3 (`this.field.m()` → the field's declared type, unique-or-drop), or T4 (globally-unique name). A
**qualified call on a named variable never guesses a same-file member** — it resolves only if the name is
globally unique, else it drops. Determinism holds (two full builds byte-identical).

### The Swift parse fix (why +88%)

tree-sitter-swift exposes a method call's callee (`navigation_expression` holding `self.foo`) as an
**unnamed** child, and the callee extractor only scanned _named_ children — so `self.method()` /
`obj.method()` calls were **dropped at parse time** and never entered the graph. Scanning all children +
a rightmost-identifier fallback recovered them (the iOS corpus went 38k → 115k captured calls), and the
receiver-aware tiers then resolved them precisely. This was a latent recall bug, not just a missing tier.

## Expected recall for the remaining phases (RANGES — confirm with counters)

Against the **addressable** bucket (calls whose name matches an internal def but is dropped for
ambiguity — 687/1646 internal names ambiguous; NOT the raw total that includes external libs):
NestJS/TS **10.2% → ~20–35%** (T3 carries it), Swift meaningfully above **25.5%** via T1+T3 + the
1029 recovered extensions. SCIP stays the escalation tier for overloads / re-export chains / anything CHA drops.
