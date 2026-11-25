# kro-rs — the Kube Resource Orchestrator, in Rust

kro-rs is a Rust port of [kubernetes-sigs/kro](https://github.com/kubernetes-sigs/kro):
you write a **ResourceGraphDefinition** (RGD), kro-rs generates the CRD for its
instances, and reconciles every instance by creating and updating the templated
child resources in dependency order, wiring data between them with CEL
expressions, and projecting child status back onto the instance.

The full-custom-controller pattern, expressed as data.

```
ResourceGraphDefinition ──compile──▶ graph (DAG + CEL programs + instance CRD)
        │                                        │
        │ accepted                               │ per-instance reconcile
        ▼                                        ▼
 instance CRD (dynamic)              children applied in topo order,
 (created at runtime)                status projected back, deletion
                                     in reverse waves
```

## Goals

1. **Standalone first.** Like upstream kro: one process, talks to any
   Kubernetes API server (a real cluster or a kapi server) over the standard
   list/watch/apply verbs. Zero required dependencies on kapi.
2. **Embeddable.** The engine is a library: hand it any implementation of the
   `Face` trait (list / watch / apply) and it runs — inside the kapi controller
   host over the same inproc lane the other controllers ride, or standalone
   over HTTP.
3. **Semantics parity** with upstream kro v0.9 on the core surface: SimpleSchema,
   template/externalRef/readyWhen/includeWhen/forEach, CEL interpolation with the
   kro function library, stable topological ordering with cycle reporting,
   instance CRD synthesis, built-in conditions + state machine, author status
   projections, wave-based deletion, orphan pruning.

## Non-goals (v1)

- GraphRevisions (upstream KREP-013): spec-hash change handling is v1-simple —
  a new spec recompiles the graph; instances reconcile against the new graph.
  Versioned immutable revisions are a follow-up.
- Patch nodes and the new `Graph` API (upstream v0.9.x): this port targets the
  classic `ResourceGraphDefinition` API.
- Full compile-time CEL type checking against OpenAPI schemas. v1 validates:
  expressions parse, identifiers resolve to known resources/schema/each, and
  template structure (identity fields, static GVK). A type checker is a
  follow-up on the same seam (`graph::compile`).

## Architecture

One workspace, three crates:

| crate | role | depends on |
|---|---|---|
| `kro` | the engine — pure laws + one async executor over [`Face`] | `cel`, `kube-core` (event envelope), `serde_json`, `tokio`, `sha2`, `md-5`, `jiff` |
| `kro-kube` | the standalone face: kube-rs client over HTTP, plus the `kro` runner binary | `kro`, `kube`, `k8s-openapi` |
| `kro-kapi` | the embed adapter: impls [`Face`] for kapi's `controller::Executor` | `kro`, `kapi/controller` (path dep) |

### The face — the whole I/O surface

```rust
pub trait Face: Send + Sync + 'static {
    fn list(&self, target: &Target) -> impl Future<Output = Result<Listed, FaceError>> + Send;
    fn watch(&self, target: &Target, rv: Option<&str>)
        -> impl Future<Output = Result<mpsc::Receiver<WatchEvent>, FaceError>> + Send;
    fn apply(&self, action: &Action) -> impl Future<Output = anyhow::Result<()>> + Send;
}
```

`Target`/`Action`/`Listed` mirror the kapi controller vocabulary one-to-one
(group/version/plural/namespace targets; Create / Patch / PatchStatus / Delete
actions; `kube_core::watch::WatchEvent<Value>` events), so the kapi adapter is a
thin rewrite, and every guarantee kapi's loop already carries (the
list→resync→watch-from-rv law, the 410 Gone heal, jittered reconnects) is
reimplemented once in the engine's loop set with the same semantics.

### The engine — one event pump, dynamic loop set

Unlike the fixed-target controllers, kro-rs learns targets at runtime: every
accepted RGD contributes an instance-kind watch and one watch per distinct child
GVR. The engine runs:

- a **pump** per GVR (list → resync replay → watch from the list's rv; 410 heals
  by re-list) that forwards events into one central channel,
- a **dispatcher** holding the state: compiled graphs (keyed by RGD
  name + spec hash), per-GVR loop handles, and a deduplicating work queue of
  instance keys,
- **workers** that drain the queue and run one async reconcile per instance
  (serialized per key — never two reconciles of the same instance in flight),
- a **beat** (2s) that re-enqueues instances sitting in not-ready backoff
  (capped exponential), re-checking readiness without an event trigger.

### Reconcile (one instance, non-deleting)

1. Stamp the finalizer (`kro.rs/finalizer`) and instance labels — compare-then-
   patch, the patch-loop guard.
2. Look up the compiled graph; absent/stale → recompile from the stored RGD.
3. Walk the topological order. Per node:
   - a dependency not yet ready → **gate** (soft not-ready; dependents wait),
   - `includeWhen` false → **skip** (contagious: dependents skip too, and the
     node's existing children prune),
   - render the template: every `${...}` CEL expression evaluates against the
     scope `{schema: <instance subset>, <resource id>: <observed>, each: <item>,
     runtime: {}}`; a whole-string expression takes the CEL value as-is, an
     embedded one stringifies,
   - `forEach` expands the cartesian product of iterator bindings (every iterator
     must appear in an identity field — metadata.name/namespace — validated at
     compile time),
   - apply: get-by-label + name → create when absent, merge-patch when the
     rendered fields differ (compare-then-patch), stamp the kro labels
     (`kro.rs/node-id`, `.owned=true`, instance identity, apply-order wave,
     `app.kubernetes.io/managed-by=kro`),
   - `readyWhen` all true → node ready, observed object published to the scope;
     otherwise the node stays pending (soft not-ready) and gates dependents.
4. Prune orphans: children found by the instance label across the graph's GVRs
   that are not in the current applied set (includeWhen flips, forEach shrink)
   — deleted only after the full apply pass resolved.
5. Project status: author `spec.schema.status` fields (CEL) merge onto
   `status`; built-in conditions (`Ready` root ⊇ `InstanceManaged`,
   `GraphResolved`, `ResourcesReady`) plus author `status.conditions` expressions
   (`newCondition(...)`) stamp with `observedGeneration` and
   `lastTransitionTime` preservation; `state` ∈ {IN_PROGRESS, ACTIVE, DELETING,
   ERROR}. The status patch is skipped when identical to the wire (the
   resourceVersion-bump loop guard).

### Deletion (one instance)

Wave-based, reverse apply order: children of the highest apply-order wave delete
first; the reconciler waits (listing by label) until the wave drains, then the
next wave; when the inventory is empty the finalizer drops. A wave that refuses
to drain keeps the instance in `DELETING` and retries on the beat.

### RGD lifecycle

- RGD event → validate + compile. Refusals surface as the RGD's status
  conditions (`GraphAccepted=False` with the violation message), never as
  controller errors.
- Accepted → ensure the instance CRD (create-if-absent via `Action::Create` on
  `customresourcedefinitions` — both kapi and real clusters serve this), then
  spawn the instance-kind pump and one pump per child GVR.
- Instance CRUD → enqueue. Child events → resolve the owning instance from the
  kro labels → enqueue (this is what drives readiness propagation).
- RGD deleted → wave-delete every instance's children, delete the instance CRD
  (kapi cascades its rows; real clusters delete the CRs too), then drop the
  compiled graph + pumps.

### CEL

Expressions are scanned out of string leaves (`${...}`, nested-brace aware),
parsed through the shared `cel` environment (cel-rust 0.16: the
cel-spec-conformant release — the bare `Program::compile` parser has no
macros and no optional syntax, so every compile goes through
`Env::stdlib()` + extensions), and the top-level identifiers are walked
out of the AST to derive the DAG.

The expression surface is upstream kro's:

- **cel-rust stdlib + extensions** (was the big parity gap — everything
  below now works): the optional operator `a.?b` / `orValue`, the
  strings library (`split`, `join`, `format`, `substring`, `charAt`,
  `indexOf`, `lowerAscii`, `upperAscii`, `replace`, `trim`, `reverse`,
  `strings.quote`), the lists library (`sort`, `sortBy`, `slice`,
  `distinct`, `flatten`, `reverse`, `lists.range`), two-variable
  comprehensions (`transformList`, `transformMap`, `transformMapEntry`,
  `all(k,v)`, `exists(k,v)`), `cel.bind`, `base64.encode`/`decode`,
  `sets.*`, and IP/CIDR (`isIP`, `cidr(...).containsIP`).
- **kro's own library, upstream shapes**: `merge`/`deepMerge`
  (member + global form, strict maps), `omit()` (sentinel only — no
  key-filtering form), `hash.fnv64a`/`sha256`/`md5` (return raw BYTES;
  hex/base64 is the author's `"%x".format` / `base64.encode` choice),
  `json.marshal`/`unmarshal`, `random.seededString(length, seed)` and
  `random.seededInt(min, max, seed)` (sha256-seeded, upstream
  algorithm), `lists.setAtIndex`/`insertAtIndex`/`removeAtIndex`,
  `runtime.newCondition({type, status, reason, message})` (a parse-time
  macro validating bare-identifier keys, `type`+`status` required) and
  `runtime.condition(obj, type)` (reads `obj.status.conditions[]`,
  empty condition when absent).
- **hand-ported k8s libraries** cel-rust lacks: `find`/`findAll`
  (regex), the `url()` family (`isURL`, `getScheme`/`getHost`/
  `getHostname`/`getPort`/`getEscapedPath`/`getQuery`), and the
  `quantity()` family (`isQuantity`, `add`/`sub`, `isLessThan`/
  `isGreaterThan`/`compareTo`, `asInteger`/`asApproximateFloat`,
  `sign`, `isInteger` — exact i128 mantissa × 10^exp arithmetic).

Still upstream-unimplemented (non-goals): CEL *type checking* against
the synthesized OpenAPI schema, GraphRevisions, patch nodes, and the
`url`/`semver` k8s-library subset (the URL getters exist; `semver` and
the k8s list aggregates `sum`/`min`/`max`/`isSorted` are absent).

### SimpleSchema → instance CRD

`spec.schema.spec` is SimpleSchema (`replicas: integer | default=1 minimum=1`,
`[]string`, `map[string]string`, custom `types`), converted to a structural
OpenAPI v3 schema — deliberately inside the subset kapi's `is_structural` law
enforces (type/properties/items/required/enum/default/pattern-subset/
preserve-unknown), so the same synthesized CRD passes kapi admission unmodified.
`spec.schema.status` fields keep their CEL expressions; the synthesized status
schema carries `state` + `conditions` (typed) and per-field types inferred from
the CEL output where trivial, else `x-kubernetes-preserve-unknown-fields`.

## Embedding into kapi (integration plan)

1. Seed `resourcegraphdefinitions.kapi.qreta.io` in `inbuilt_crds()` (typed, with
   x-kubernetes-validations admission rules) — auto delete-guarded.
2. `spawn_embedded_controller` + `Cmd::Controller`: spawn
   `kro_kapi::run(io)` where `io` is the same `SharedExecutor` the other faces
   ride (fabric inproc lane embedded, HTTP standalone).
3. The RGD seed makes the group `kapi.qreta.io/v1alpha1`; instance CRDs default
   their group to it. Both ride the generic CR surface; instance CRDs register
   at runtime via `Action::Create` on `customresourcedefinitions` (already works
   end-to-end: discovery, CR paths, /status, watches light up with no restart).

## Testing

- Unit: dag stability, simpleschema conversion, expression scanning/identifier
  extraction, graph validation refusals, CRD synthesis shape, CEL evaluation +
  function library, conditions stamping, wave computation.
- Integration (dev-dep on kapi-server): boot the real axum server on
  `127.0.0.1:0` with an in-memory store, drive the standalone `kro-kube` face
  against it — RGD admission, instance CRD registration, child creation order,
  readiness gating, includeWhen pruning, forEach expansion, deletion waves.
  This doubles as the kapi-embed proof: the same face shape the fabric lane
  serves.
