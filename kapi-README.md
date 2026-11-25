# kapi

Kubernetes-shaped algorithms and serving crates for the qreta stack. Semantics live in small, testable crates; serving and I/O stay in the arms.

## Layout

| Path | Crate/package | Purpose |
| --- | --- | --- |
| `crates/core` | `kapi-core` | Pure, sans-I/O algorithms: CRD/schema/apply law, discovery, job/cronjob state machine, and shared data types. |
| `crates/server` | `kapi-server` | API server library: request routing/dispatch, object store integration, watch feed, HTTP face (`api`), embedded SQLite store, replica/restore, and inbuilt Kubernetes resources. |
| `crates/kubelet` | `kubelet` | Node-side abstractions and kube-rs-backed client/executor integration. |
| `crates/controller` | `controller` | Controllers over the kubelet-shaped loop (scheduler, node lifecycle, garbage collection). |
| `src/` | `kapi` | Workspace root package: re-exports `kapi-core` as `kapi::*` and ships the one deploy binary (`kapi server` / `kapi controller`). |

The gateway is now a separate project and binary at `~/src/gateway` (host
routing, edge cache, uniflight-based request coalescing). It dials this
server's API as its route-table source of truth; the old `kapi gateway`
subcommand and `--with-gateway` embedded arm are gone.

```text
kapi-core (pure semantics)
   ├─> kapi-server (apiserver + store + HTTP/watch)
   ├─> kubelet (node/client/executor)
   └─> controller (reconcilers)
kapi (one binary: `server` / `controller` subcommands) -> server/controller entrypoints

the gateway is a separate project (~/src/gateway) -> dials kapi's API for routes
```

## Server behavior (current)

- Serves core discovery/version and API-group/resource discovery for registered CRDs.
- Serves inbuilt core resources (including `pods`, `nodes`, `events`) and inbuilt CRDs seeded at startup (for example coordination leases and workload kinds under `apps`/`batch`) over the generic CR/custom-resource surfaces.
- Supports object CRUD, status subresources, list/watch semantics (including bookmarks and `timeoutSeconds`), Table responses where requested, field selectors, and pod log read/push endpoints.
- Exposes `OpenAPI` v3 discovery (`/openapi/v3`) and minimal per-group-version documents (`/openapi/v3/api/v1`, `/openapi/v3/apis/{group}/{version}`) as the primary `OpenAPI` surface, plus a minimal Swagger 2.0 document (`/openapi/v2`) for legacy clients. Protobuf `OpenAPI` requests are refused with 406 so clients fall back to JSON.
- Stores state in SQLite (`store` module); can restore/replicate via LTX targets when configured.

## Native kinds — Function and DurableObject

Two inbuilt CRDs in the `kapi.qreta.io/v1alpha1` group carry native
(process, not wasm) workloads (KAPI-FR-101..112, `specs/native.feature`):

- **`Function`** — an ON-DEMAND OCI-native process function, defined as a
  **pure RGD** (`manifests/rgd/function.yaml`) — no custom controller. The kro
  engine registers `functions.kapi.qreta.io/v1alpha1` at runtime and its
  graph creates a core/v1 **Service** (the service definition), a typed
  gateway-api **HTTPRoute** (routes the gateway reads), and a single-instance
  apps/v1 **Deployment** that runs the process. Function status (ready/reason)
  is projected by CEL from the Deployment's `availableReplicas`. The pod
  template carries `terminationGracePeriodSeconds` so CRI-O's sandbox stop
  delivers SIGTERM on unload. Horizontal scale-to-zero rides a standard HPA
  on the `requests-per-second` custom metric (minReplicas 0 on k8s v1.37+).
- **`DurableObject`** — a global single instance with one private durable
  database, keyed by object identity. Functions bind it by name; one
  coordination Lease per object is the single-writer authority. Idle objects
  scale down without losing their state.

Both controllers are registered in `kapi controller` and the embedded
controller. Discovery, watch, status subresources and the delete guard are
generic — no per-kind plumbing outside the CRD seeds and the controllers.

Gateway routing rides the HTTPRoute's backendRef (the Service name); the
Running Pods that the Deployment's ReplicaSets create are the backend
binding the gateway reads. Idle scale-down drives those pods away through
the standard workload/CRI chain, so no bespoke process runner is involved.

Apply the RGD with `kubectl apply -f manifests/rgd/function.yaml`; kro then
creates the instance CRD and runs the graph.

## Specifications

The laws live in [`specs/`](specs/README.md) — Gherkin, one tag per
requirement ID, every Scenario naming the test that pins it (owner
directive 2026-11-16: the specs moved local; the upstream drive copies
retire when the drive session returns).

## Binaries and commands

```sh
# run the API server
cargo run --bin kapi -- server --db kapi.db --port 8643

# run tests
cargo test

# lint
cargo clippy --all-targets --all-features

# format check
cargo fmt --all --check
```

`justfile` contains deploy/build helpers for the Linux musl targets used on the VM. See `just --list`.

## Notes

- The root `kapi` crate keeps the `kapi::` API path stable by re-exporting `kapi-core`.
- `stress/` contains local load/stress scripts (for example `stress/nodes.js`) and is not part of the crate build.
