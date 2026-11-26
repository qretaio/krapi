# kro × kapi — the integration review (historical; workspace merged 2026-11-22)

What it takes to run the kro engine inside the kapi controller process,
and what it takes to keep it standalone. Read with [DESIGN.md](DESIGN.md)
(the engine's own architecture); this file is the boundary.

## The one-paragraph summary

kro was a standalone workspace with two carriers: `kro-kube` (the `kro`
binary, kube-rs over HTTP — any Kubernetes-shaped server, a real cluster
or kapi) and `kro-kapi` (the embed, the engine's [`Face`] over the kapi
controller's `SharedExecutor`). The embed is a **zero-rewrite
pass-through**: the engine and kapi's controller share the same
`WatchEvent` type and the same `Target`/`Action` vocabulary, so the
adapter only re-spells crate paths. Integration into kapi is two moving
parts — a CRD seed and one `spawn` call — plus four law interactions
this document pins down.

## What runs where

```
standalone:   kro (binary) ──HTTP/kube-rs──▶ any API server
embedded:     kapi server process
                └─ spawn_embedded_controller
                     ├─ kapi controllers (Scheduler, Node, GC, Fleet, PublicAssets)
                     │    via the Tokio channel lane
                     └─ kro engine (its own pumps + workers)
                          via the same SharedExecutor
```

The kro engine does NOT ride kapi's `Controller` trait. kapi's loop
samples `targets()` once per controller; kro's kinds are dynamic (the
RGD declares them at runtime), so kro runs its own pump set — one list →
resync → watch loop per surface — over the same executor. That is a
feature, not a fork: the engine runs identically embedded and standalone
(the E2E drives the standalone carrier against a booted kapi server and
exercises every law the embed will hit).

## The integration (landed)

All three steps are in, exactly as designed:

1. **The dependency** — kapi's workspace table:
   `kro-kapi = { path = "crates/kro-kapi" }` in the merged kapi
   workspace, referenced from the server crate. The old kro-rs repo
   was dropped; its crates live in `crates/kro*` here. `kro-kapi` re-exports `EngineConfig` (and the
   label constants) so the host needs one crate only.

2. **The RGD seed** — `apiserver::rgd_crd()` in `inbuilt_crds()`:
   `resourcegraphdefinitions.kro.run`, cluster-scoped, spec AND status
   `x-kubernetes-preserve-unknown-fields` (the engine writes its own
   status projection through the status subresource). The seed buys the
   delete-guard protection and discovery registration for the RGD kind
   itself; the **instance kinds stay runtime-registered** (kro creates
   them via `Action::Create` on `customresourcedefinitions`, which both
   E2Es prove through the live CR surface).

3. **The spawn** — in `spawn_embedded_controller` (the
   `--with-controller` arm), beside the kubelet controllers:
   ```rust
   kro_kapi::spawn(
       controller::SharedExecutor::erase(executor.clone()),
       kro_kapi::EngineConfig::default(),
   );
   ```
   Dropping the returned `JoinHandle` detaches the engine's task
   (tokio's spawn semantics) — it runs beside the kubelet loops, never
   blocking them. `EngineConfig::default()` is workers 4, tick 2s,
   backoff 2s→60s, the `kro.run`/`v1alpha1` defaults.

No other changes. The proof is `crates/kro-kapi/tests/embed.rs`: it boots
the real server (`seed_inbuilt_crds` + `hub::serve_channel` + the axum
router — the boot law verbatim), spawns the engine over the Tokio channel lane, and drives the full lifecycle through
HTTP: accept, runtime instance-CRD registration, ordered children,
ACTIVE + projection + Ready fold + finalizer, wave deletion, drain.

## The law interactions (pinned, all E2E-proven)

* **The drain law** — kapi's delete stamps `deletionTimestamp` and keeps
  the row; a SECOND delete purges. kro's `deletion_plan` emits the
  finalizer drop **and** the second DELETE in one plan, so a kro-managed
  instance drains in one pass once its children are gone.
* **The patch-never-creates law** — kapi's `cr_patch` 404s an explicit
  `merge-patch+json` on a missing row. The drain's finalizer patch can
  race the purge (two workers) and 404; the worker logs it as an apply
  failure and the row stays dead — the correct outcome. Only
  apply-flavored and bare-intent patches create.
* **The status wrapper** — kro's status patches ride kube-rs' subresource
  shape (`{"status": …}`); kapi's `cr_status` accepts both that and the
  raw status object.
* **The echo law** — every kro write echoes a watch event back into the
  engine. Every status write in the engine is gated on a diff
  (status-diff for RGDs, the merged-result compare for instances), so
  echoes converge instead of feeding themselves.

## The open questions for the owner

1. **The instance kinds and the delete guard** — the guard derives from
   the seeds; runtime-registered instance kinds are not protected from a
   CRD delete. Acceptable (kro re-creates them on the next RGD event),
   or the guard should grow a runtime registry?
2. **Worker count** — default 2. kro's passes are I/O-bound (applies);
   the pump fan-out is per-GVR. Tune via `EngineConfig` after a real
   graph lands.
3. **The kro.run group** — a normal CRD group (not reserved), so the seed
   rides the standard path. If the group should ever change, it is one
   constant (`EngineDefaults::instance_group`) plus the seed's group.
4. **Standalone kro against the kapi server** — supported today (the
   E2E IS that configuration). The runner binary's token/namespace
   arguments match the kapi server's bearer auth.
