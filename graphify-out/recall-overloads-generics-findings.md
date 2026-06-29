# R&D — Recall on Overloads / Generics / Dynamic Dispatch _Without_ a Compiler

**Question:** How far can CodeGraph's syntactic (no-compiler) CALL resolver safely go on overloads (same name, different arity/types), generics, trait/protocol/interface dispatch, and virtual dispatch — **without emitting a phantom edge**? Where MUST it drop? Reference CHA vs RTA vs VTA. Is arity-matching safe? What is the dynamic-dispatch ceiling?

**Brand constraint (non-negotiable):** unique-or-drop, **zero phantom edges**, determinism sacred. Every technique below is judged first on _can it create a wrong edge or a non-deterministic one?_ If yes → reject or gate behind AMBIGUOUS-drop.

---

## 0. TL;DR (the adoptable boundary)

| Construct                                                     | Safe syntactic move                                                                                                           | Verdict                                                                                                                                 |
| ------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| **Overload by arity** (same name, different param count)      | Filter candidates by arg count; if exactly one survives → edge, else DROP. **Arity model MUST err wide (§2).**                | ✅ **Adopt with err-wide guardrail** — a _mismodeled-narrow_ arity + same-named decoy CAN mint a phantom; err-wide closes it. S effort. |
| **Overload by type** (same name+arity, different param types) | Would need argument _type_ inference → not available syntactically                                                            | ❌ **Drop to AMBIGUOUS** unless arity already made it unique                                                                            |
| **Generics / templates**                                      | Resolve on the _erased_ / declared method name; ignore type args                                                              | ✅ Already safe — generics don't add call targets, they parameterize them. Type args are irrelevant to _which method_ is named.         |
| **Virtual / trait / protocol / interface dispatch**           | Resolve to the **declared-type** method **only if** the type cone collapses to one definition (T0/T4 unique). Otherwise DROP. | ⚠️ **This is the ceiling.** CHA's "edge to every override" is the _opposite_ of unique-or-drop. Never enumerate the cone.               |
| **`this.field.method()` (DI)**                                | Resolve field's declared type → method, if unique (existing T3)                                                               | ✅ Keep — declared-type single-hop, no runtime guess                                                                                    |

**One-line thesis:** _Arity-matching is the single highest-value safe recall win and pairs with unique-or-drop — **provided the arity model errs wide** (§2); a mismodeled-narrow arity beside a same-named decoy is the one way it can mint a phantom, so err-wide is mandatory. Everything past arity (argument-type overloads, virtual cone expansion) is where a sound call-graph algorithm (CHA/RTA) would ADD many edges — exactly the phantom-edge sin CodeGraph forbids. CodeGraph's brand is the **dual** of CHA: CHA over-approximates (sound, imprecise); CodeGraph under-approximates (unsound by design, precise **under the closed-world assumption**). They are designed to fail in opposite directions._

---

## 1. The literature: CHA → RTA → VTA, and why their _direction_ is wrong for us

The classic call-graph family is built to be **sound** — it must contain _every_ edge possible at runtime, accepting false edges (imprecision) to never miss one. CodeGraph is built for the inverse property. Knowing the family precisely tells us exactly which of their moves we may borrow and which we must invert.

### Class Hierarchy Analysis (CHA)

At a virtual call `recv.m(...)`, CHA takes the **declared (static) type** `T` of `recv` and emits an edge to `m` in **every** subtype of `T` in the class hierarchy that defines/inherits a matching `m`.

- Source: Dean, Grove, Chambers, _"Optimization of Object-Oriented Programs Using Static Class Hierarchy Analysis"_ (ECOOP 1995).
- Bacon & Sweeney measured CHA resolving (to a single target) only **~51%** of C++ virtual calls — the rest fan out to multiple targets. ([Bacon & Sweeney, _Fast Static Analysis of C++ Virtual Function Calls_, OOPSLA'96 — author PostScript](http://web.cs.ucla.edu/~palsberg/tba/papers/bacon-sweeney-oopsla96.ps))
- **For us:** CHA _adds_ an edge per override. That is N phantom-risk edges when the runtime only ever hits one. **We cannot adopt CHA's cone expansion.** We can only borrow its _declared-type lookup_ and keep the edge **iff the cone has exactly one definition** (no override, or a final/sealed/sole implementor).

### Rapid Type Analysis (RTA)

RTA refines CHA by intersecting the type cone with the set of types **actually instantiated** (`new T()`) anywhere in the whole program. Bacon & Sweeney: RTA resolves **~71%** of C++ virtual calls vs CHA's 51%. ([Bacon & Sweeney OOPSLA'96](https://dl.acm.org/doi/10.1145/236337.236371))

- **For us:** RTA needs a **whole-program instantiated-type set**. That is global, and it can change non-locally when a `new` is added in a far file → **determinism risk** and cross-file dependency. The same-commit→byte-identical guarantee survives only if the instantiation set is computed deterministically over the _whole_ graph. Even then, RTA still _expands_ to the surviving cone, so it shares CHA's phantom direction. **Reject for default; see §6 for an optional gated mode.**

### Variable Type Analysis (VTA) & the propagation family (CTA/MTA/FTA/XTA, 0-CFA)

VTA (Sundaresan et al., OOPSLA'00) and the Tip & Palsberg propagation family assign **type sets to variables/fields/methods** and propagate them across assignments to shrink each call site's receiver set. ([Tip & Palsberg, _Scalable Propagation-Based Call Graph Construction Algorithms_, OOPSLA'00](https://dl.acm.org/doi/10.1145/353171.353190) · [author PDF mirror](http://web.cs.ucla.edu/~palsberg/paper/oopsla00.pdf))

- The family forms a precision/cost ordering roughly **CHA ≤ RTA ≤ {CTA,MTA,FTA,XTA} ≤ 0-CFA ~ VTA** (later = more precise, more expensive), all **sound over-approximations**. (The exact placement of VTA relative to 0-CFA is debated in the literature and reconstructed here from secondary sources — treat as "comparable," not a strict order. The precise internal ranking doesn't affect our conclusion: the whole family is the "must-drop" boundary because it needs flow analysis we don't run.)
- They all require **inter-procedural dataflow / points-to propagation** — exactly the compiler-grade machinery CodeGraph deliberately doesn't run.
- **For us:** out of scope. Not because they're imprecise (they're the precise end) but because they need flow analysis we don't do, and they still over-approximate at residual call sites.

**The key reframing for the write-up:** Tip & Palsberg's hierarchy is a tower of _increasingly precise over-approximations_. CodeGraph sits **below CHA** on a different axis entirely: it's an **under-approximation** (drops on doubt). The papers tell us the _shape of the doubt_ (declared-type cone, instantiation set, points-to) so we know precisely when our cheap syntactic signal is provably unique and when it is not.

| Algorithm           | Info needed                     | Direction           | Phantom risk        | CodeGraph fit                                   |
| ------------------- | ------------------------------- | ------------------- | ------------------- | ----------------------------------------------- |
| Name-only           | call-site name                  | over-approx         | **high** (homonyms) | ❌ (this is the codebase-memory fuzzy mode, §4) |
| **CHA**             | declared type + class hierarchy | over-approx (cone)  | medium              | borrow _lookup_, invert _cone_ → unique-or-drop |
| RTA                 | + whole-program `new` set       | over-approx         | medium              | gated optional only (§6)                        |
| 0-CFA/VTA/XTA       | + inter-proc points-to          | over-approx (tight) | low                 | ❌ needs flow analysis                          |
| **CodeGraph T0–T4** | local syntactic, unique-or-drop | **under-approx**    | **zero**            | ✅ the brand                                    |

---

## 2. Is arity-matching safe? (Yes — and it's the best win)

**Claim: arity-matching is phantom-free _iff_ the arity model errs in the sound (widening) direction. Under that governing rule it is a pure recall gain. Get the direction wrong and it _will_ emit a phantom edge — so the rule is mandatory, not advisory.**

**First, the assumption everything rests on — closed-world.** CodeGraph's "zero phantom" is already conditional on the **closed-world assumption**: the true call target is a definition _present in the graph_. T4 global-unique can itself emit a wrong edge against an out-of-graph homonym (e.g. a stdlib/3rd-party method not indexed). Arity inherits exactly this assumption — it adds no _new_ unsoundness, but it does not escape the existing one. Honest framing throughout: **"phantom-free under the closed-world assumption (true target in-graph); arity preserves this _iff_ the model errs wide."**

Reasoning, under closed-world. Arity matching only ever **removes** candidates from a set the resolver already had. The resolver's rule is _unique-or-drop_. Outcomes when the arity model is **correct**:

1. Candidate set was already 1 → filter leaves it 1 (or 0). No change in edge correctness; possibly catches a mismatch.
2. Candidate set was N>1 (currently → DROP) → filter narrows to exactly 1 → **edge recovered that was previously dropped**. Recall gain, no precision loss: the survivor is the _only_ in-graph definition whose signature can accept this call shape.
3. Candidate set narrows to 0 → still DROP. Safe.

**The phantom hole — a _mismodeled-narrow_ arity + a same-named decoy sibling.** This is the case the naive "narrowing can't add edges" intuition misses:

> Two defs named `handle`: `A.handle(a, b=1)` (true range `[1,2]`) and `B.handle(a, b)` (`[2,2]`). Suppose tree-sitter **mismodels** A's default and stores A as `[1,1]` (it saw the param, missed the `=1`). Call `recv.handle(x, y)` (2 args). The filter now **excludes A** (thinks max=1), keeps **B** → unique survivor → **edge to B**. If `recv` was actually an `A`, that is a **phantom edge** — a wrong edge minted by a modeling error, violating the sacred constraint.

So a mismodel does **not** always degrade gracefully to a drop. When a sibling decoy survives, mismodel-narrow → wrong-unique → phantom. The earlier intuition only covered the _zero-survivor_ mismodel (→ drop, safe); the _decoy_ case is the dangerous one.

**Governing rule (mandatory) — ERR WIDE, NEVER NARROW.** When uncertain about a default / optional / variadic / rest / spread param, **widen `[min, max]`; never tighten it. Unknown variadic-ness ⇒ `max = ∞`. Unknown optionality ⇒ lower `min`.** A too-wide range costs only _recall_ (more survivors → ambiguous → drop, brand-safe). A too-narrow range costs _precision_ (excludes the real target, lets a decoy win → phantom, brand-fatal). The asymmetry is the whole safety argument: under err-wide, the only way an edge is emitted is genuine, conservative arity-uniqueness — so the N→1 recall claim above holds soundly.

**Modeling caveats (all resolved by err-wide):**

- **Variadics / defaults / optional / rest:** model declared arity as `[minRequired, maxAccepted]` (`max = ∞` if variadic/rest), keep candidate iff `callArgc ∈ [min, max]`. When the parser can't tell → widen.
- **Keyword/named arguments (Python, Kotlin, Swift labels):** raw positional count under-counts. Swift's argument _labels_ are part of the selector — they make resolution _easier_ (label set ≈ signature). Python `**kwargs` makes positional count a loose lower bound only → treat as `max = ∞` when `**kwargs`/`*args` present. Per-language arity predicate, always erring wide.
- **Determinism:** arity is a pure function of syntactic node counts → byte-identical across runs. ✅

**Verdict:** **Adopt arity as a candidate filter inside the existing tiered resolver, before the unique-or-drop check — with the err-wide rule enforced in every language's parameter model.** Effort **S**. Under err-wide it strictly increases recall on arity-separable overloads while preserving zero-phantom (closed-world) + determinism. This is the headline recommendation; the err-wide rule is the non-negotiable condition that makes it true.

---

## 3. Generics / templates — already safe, no special handling needed

A generic/templated method `foo<T>(x: T)` is **one** call target regardless of how `T` is instantiated. Type arguments select _types_, not _methods_ (in the absence of C++-style template specialization or trait-bound dispatch). Therefore:

- **Resolve on the bare method name + arity, ignore type arguments.** No phantom risk; type args are noise for _which-method_ questions.
- **Exception — C++ explicit/partial template specialization** and **Rust trait-bound monomorphization picking a different `impl`**: here the type argument genuinely selects a _different body_. That collapses into the dynamic-dispatch problem (§5) — you cannot know the chosen specialization syntactically → **DROP if more than one candidate impl exists.** Single `impl`/no specialization → unique → keep.
- **Determinism:** fine.

**Verdict:** No new work for the common case; generics fold into existing name+arity resolution. Trait-bound/specialization selection folds into §5's drop rule. Effort **S** (mostly "don't be fooled by `<...>` tokens").

---

## 4. Competitor data point: where guessing goes wrong

The directly-comparable competitor — a tree-sitter knowledge-graph builder ([_Codebase-Memory_, arXiv 2603.27277](https://arxiv.org/html/2603.27277v1)) — uses a **6-strategy confidence cascade** and **accepts guesses down to confidence 0.30** (fuzzy string similarity) rather than dropping. Strategies 4–6 are: "unique project-wide name" (0.75), "suffix match via import-distance scoring" (0.55), "fuzzy string similarity" (0.30–0.40).

- Strategy 4 ("unique name", 0.75) ≈ CodeGraph's **T4 global-unique** — the _only_ one of their low-confidence tiers that is phantom-safe, and CodeGraph already has it.
- Strategies 5–6 (import-distance scoring, fuzzy) are **exactly the phantom-edge generators** CodeGraph forbids. They will, by construction, emit wrong edges on homonyms and are **non-deterministic if the scoring has any tie-break by float ordering**.

**Sourcegraph** draws the same line institutionally: **precise (SCIP, compiler-backed) navigation "is not susceptible to false positives… from symbols with the same name"**, while **search-based navigation "can return false-positive results"** and is the explicit fallback. ([Sourcegraph: precise code navigation](https://sourcegraph.com/docs/code-search/code-navigation/precise_code_navigation)) The overload example they cite: C++ `find`/`contains` across container types — only the compiler-precise index disambiguates the correct overload; search-based produces false positives.

**Takeaway for brand:** CodeGraph's "drop instead of guess" is _the same posture as Sourcegraph's precise tier_, achieved without a compiler by simply refusing the cases SCIP would need the compiler for. The competitor that guesses (Codebase-Memory ≥0.30) is the cautionary tale. Our differentiator is **"every edge is a precise edge,"** and arity-matching extends how many precise edges we can claim _without crossing into the guess zone._

---

## 5. The dynamic-dispatch ceiling (declared type vs runtime type)

This is the hard wall. At `recv.m()`:

- **Static / declared type** of `recv` is the most a syntactic resolver can ever know: "`recv` is declared `Animal`."
- **Runtime type** is what actually dispatches: `Dog`, `Cat`, … — undecidable in general, and _not even bounded_ without the instantiation set (RTA) or points-to (VTA).

The ceiling theorem (folklore, formalized across the CHA→VTA literature; see [Tip & Palsberg OOPSLA'00](https://dl.acm.org/doi/10.1145/353171.353190) and the [Holland explainer](https://ben-holland.com/call-graph-construction-algorithms-explained/)):

> A call `recv.m()` where `recv` has declared type `T` and `T.m` is **overridden** by ≥1 subtype has a runtime target that is **not determined by any amount of local syntactic information.** Sound resolution requires enumerating the override cone (CHA) → many targets; precise resolution requires whole-program flow (VTA) → expensive.

**CodeGraph's only sound move at this ceiling: collapse-or-drop.**

- The declared-type method `T.m` is the **unique** dispatch target **iff** `m` is _not overridden_ anywhere — i.e. the type is `final`/`sealed`, the method is `final`/non-`virtual`/non-`open`, or there is exactly one implementor of the interface/trait/protocol in the whole graph (a deterministic, g-computable fact). In those cases → **keep the single edge** (this is sound _and_ precise — it's monomorphic dispatch).
- If the override/implementor cone has ≥2 members → **DROP.** Do **not** enumerate the cone (that's CHA's over-approximation = phantom edges by our definition). Do **not** pick the declared-type one as "probably right" (it often isn't — abstract base, interface with no body).

**Concrete ceiling rules per dispatch flavor:**

| Dispatch                                    | Keep edge when…                                                    | Else |
| ------------------------------------------- | ------------------------------------------------------------------ | ---- |
| Class virtual method                        | method/class `final`/`sealed`, or method never overridden in graph | DROP |
| Interface / protocol / trait method         | exactly **one** implementor of that interface in the whole graph   | DROP |
| Abstract method                             | never (no body at declared type)                                   | DROP |
| `this.field.method()` (DI)                  | field's declared type's method is unique (existing T3)             | DROP |
| Function pointer / closure / `Callable` var | never resolvable syntactically                                     | DROP |

"Exactly one implementor in the whole graph" is computable deterministically (count impls of interface X), but it is a **global** fact → if you adopt it, it must be derived from the finished graph in a deterministic pass so the same commit yields the same answer. (Adding a second implementor in a later commit correctly flips that edge to DROP — that's _correct_ behavior, not non-determinism; determinism is per-commit, not across commits.)

---

## 6. Optional gated "sole-implementor" mode (the one safe recall stretch)

The only place to push recall past current tiers **without** a phantom edge:

**Sole-implementor / sole-override devirtualization.** For an interface/trait/protocol/abstract method, if a deterministic whole-graph pass finds **exactly one** concrete implementation, that call is provably monomorphic → emit the edge. This is RTA-flavored (it uses a global fact) but **stays under-approximating**: it only fires when the cone is a singleton, so it can never add a _wrong_ edge.

- **Precision risk:** none (singleton cone = the runtime target, guaranteed).
- **Determinism risk:** none _if_ computed in a deterministic post-pass over the byte-identical graph (sort implementors, count, require ==1). Must NOT depend on file iteration order.
- **Effort:** **M** (need an interface→implementors index + a resolution pass that runs after the per-file graph is built; CodeGraph already builds CHA-style type info for T3/T4, so the index is largely there).
- **Expected gain:** meaningful on DI-heavy codebases (Spring/Nest/Swift-protocol-oriented) where most interfaces have exactly one impl in the repo. This is precisely the pattern T3 was built for, extended from "this.field" to "any interface-typed receiver."

This is the _only_ RTA-adjacent idea worth adopting, and only because the singleton-cone restriction keeps it under-approximating.

---

## 7. Rust + tree-sitter implementation sketch

All of this is local AST work except §6 (one global index).

**(a) Arity capture (per definition), §2 — effort S**

- In each language's tree-sitter query, alongside the existing function/method capture, capture the parameter list node. Compute `(minRequired, maxAccepted)`:
  - `minRequired` = count of params without default + non-rest.
  - `maxAccepted` = ∞ if a variadic/rest/`*args` param exists, else total param count.
  - Swift: also capture argument **labels** (selector pieces) — store as part of the symbol key; they disambiguate better than arity alone.
- Store on the `FunctionDef` node: `arity_min: u16, arity_max: u16 (u16::MAX = variadic)`.

**(b) Call-site arg count, §2 — effort S**

- At each call expression, count argument nodes (`call_expression > arguments`). Python/Kotlin/Swift: count positional vs labeled separately.
- In the resolver, _before_ the unique-or-drop decision, filter candidates: keep `c` iff `call_argc ∈ [c.arity_min, c.arity_max]` (and labels match, where the language has them).
- If the filtered set is size 1 → edge (with a new tier tag, e.g. `T2_ARITY`). Size ≠ 1 → existing DROP.

**(c) Generics, §3 — effort S**

- In call-name extraction, strip the type-argument node (`type_arguments` / `<...>`) so `foo::<T>()`/`foo<T>()` resolves as `foo`. Already mostly true; just ensure the generic token doesn't poison the name key.
- Rust: do **not** attempt to pick a trait `impl` by bound. If `Trait::method` has >1 `impl` → DROP (§5).

**(d) Dynamic-dispatch ceiling, §5 — effort S (it's mostly _removing_ would-be edges / asserting DROP)**

- Tag each method def with `is_open: bool` (overridable): `virtual`/`open`/non-`final` class method, or any interface/trait/protocol method.
- At a virtual call: resolve declared-type method; if `is_open` and not provably-unique → DROP. This is the guardrail that keeps a future contributor from "helpfully" enumerating the cone.

**(e) Sole-implementor pass, §6 — effort M**

- Build `impls: HashMap<InterfaceSymbolId, SmallVec<ConcreteImplId>>` during graph construction (deterministic insertion, then `sort`).
- Post-pass: for each unresolved interface-typed virtual call, look up implementors; if `len()==1` → emit edge tagged `T5_SOLE_IMPL`; else leave DROP.
- Run **after** the per-file graphs merge, over a sorted node order, so output is byte-identical.

**Determinism checklist (apply to every tier):** sort before count; no `HashMap` iteration into edge output; tie = drop, never first-wins by hash order; the global passes consume the already-deterministic merged graph.

---

## 8. Recommendations, ranked

| #   | Technique                                                                           | Precision risk                                   | Determinism risk           | Effort | Expected gain                                                                      |
| --- | ----------------------------------------------------------------------------------- | ------------------------------------------------ | -------------------------- | ------ | ---------------------------------------------------------------------------------- |
| 1   | **Arity-matching candidate filter, err-wide** (§2)                                  | none **iff err-wide**; mismodel-narrow → phantom | none                       | **S**  | **High** — recovers arity-separable overload edges currently dropped; flagship win |
| 2   | **Generics: strip type-args, resolve bare name** (§3)                               | none                                             | none                       | **S**  | Medium — removes a class of spurious drops on `foo<T>()`                           |
| 3   | **Dynamic-dispatch ceiling guardrail** (`is_open`→DROP unless provably unique) (§5) | none (prevents future phantom edges)             | none                       | **S**  | Defensive — protects the brand as languages/contributors grow                      |
| 4   | **Sole-implementor devirtualization pass** (§6)                                     | none (singleton cone only)                       | none if post-pass sorted   | **M**  | Medium–High on DI/protocol-heavy repos                                             |
| 5   | ~~CHA cone enumeration~~                                                            | **HIGH — phantom edges**                         | —                          | —      | ❌ **Reject** — violates the brand outright                                        |
| 6   | ~~RTA/VTA/points-to~~                                                               | over-approx + needs flow analysis                | RTA global set risk        | L      | ❌ **Reject** — wrong tool, wrong direction                                        |
| 7   | ~~Fuzzy/confidence-scored guessing (Codebase-Memory ≥0.30)~~                        | **HIGH**                                         | **HIGH** (float tie-break) | —      | ❌ **Reject** — the explicit anti-pattern                                          |

**Adopt 1–3 now (all S, all zero-risk). Consider 4 as a follow-up (M).** Never 5–7.

---

## 9. Honest precision/recall framing for marketing & docs

- **Don't claim soundness.** CodeGraph is **deliberately unsound** (it drops). The correct claim is **"precision ≈ 1.0 by construction under the closed-world assumption (true target in-graph); recall is honest, not inflated."** That is the _opposite_ guarantee from CHA/RTA (which are sound, imprecise). The closed-world caveat is the one residual phantom source even at T4/arity — be upfront about it rather than claiming an absolute zero.
- Bacon & Sweeney's numbers are the honest yardstick: even _sound_ CHA only pins ~51% of C++ virtual calls to a single target; RTA ~71%. The remaining ~30–50% are genuinely polymorphic — **any** tool claiming a single precise edge there without a compiler is guessing. CodeGraph dropping them is the _correct_ answer, and arity-matching legitimately reclaims the subset that polymorphism never actually made ambiguous (different arities).
- Position against Sourcegraph: CodeGraph delivers **SCIP-precise-tier behavior (no same-name false positives) without the compiler/build step**, by refusing exactly the cases SCIP needs the compiler for — and now reclaiming the arity-separable overloads on top.

---

## Sources

- Tip & Palsberg, _Scalable Propagation-Based Call Graph Construction Algorithms_, OOPSLA 2000 — [ACM](https://dl.acm.org/doi/10.1145/353171.353190) · [author PDF](http://web.cs.ucla.edu/~palsberg/paper/oopsla00.pdf)
- Bacon & Sweeney, _Fast Static Analysis of C++ Virtual Function Calls_, OOPSLA 1996 — [author PostScript (UCLA mirror, confirmed via search)](http://web.cs.ucla.edu/~palsberg/tba/papers/bacon-sweeney-oopsla96.ps) (CHA 51% / RTA 71% single-target resolution; ACM DOI not independently verified, so the stable author mirror is cited instead)
- Dean, Grove, Chambers, _Optimization of OO Programs Using Static Class Hierarchy Analysis_, ECOOP 1995 (origin of CHA)
- Sundaresan et al., _Practical Virtual Method Call Resolution for Java (VTA)_, OOPSLA 2000
- Ben Holland, _Call Graph Construction Algorithms Explained_ — [ben-holland.com](https://ben-holland.com/call-graph-construction-algorithms-explained/) (CHA≥RTA≥VTA precision/soundness ordering; overload-vs-override; declared-type lookup)
- Creager et al., _Stack Graphs: Name Resolution at Scale_, EVCS 2023 — [arXiv 2211.01224](https://arxiv.org/abs/2211.01224) · [GitHub blog](https://github.blog/open-source/introducing-stack-graphs/) (file-incremental, declarative name binding — the precise-without-build comparator)
- _Codebase-Memory: Tree-Sitter-Based Knowledge Graphs…_, arXiv 2603.27277 — [arXiv](https://arxiv.org/html/2603.27277v1) (6-strategy confidence cascade, guesses to 0.30 — the phantom-edge anti-pattern)
- Sourcegraph, _Precise Code Navigation_ — [docs](https://sourcegraph.com/docs/code-search/code-navigation/precise_code_navigation) (precise tier has no same-name false positives; search-based does — the institutional precise-vs-fuzzy line)
- _NoCFG: A Lightweight Approach for Sound Call Graph Approximation_ — [arXiv 2105.03099](https://arxiv.org/pdf/2105.03099) (soundness/precision/scalability trade-off framing)
