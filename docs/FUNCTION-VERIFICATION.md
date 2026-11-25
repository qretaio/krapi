# Function RGD — qreta-dev verification (2026-11-23)

## Verified on qreta-dev (kapi rebuilt + deployed, binary 2026-11-23)

- RGD `manifests/rgd/function.yaml` applied via POST /apis/kro.rs/v1alpha1/resourcegraphdefinitions → 201.
- Status: `GraphAccepted=True`, `KindReady=True`, `ControllerReady=True`, state `Active`, topologicalOrder `[functionService, functionRoute, functionDeployment]`.
- Instance CRD `functions.kapi.qreta.io` registered at runtime by the kro engine.
- Function instance `dev/hello` (image, command=[/bin/worker], args, env, routes, defaults idleSeconds=60/privateDb=true) → children created: Service `v1/dev/hello`, Deployment `apps/v1/dev/hello`, HTTPRoute `gateway.networking.k8s.io/v1/dev/hello-hello.qreta.dev`.
- Command/args/env are native JSON arrays/maps on the Deployment container (whole-string `${...}` takes the CEL value as-is).
- Function status after the controller-fix rebuild: state `ACTIVE`, conditions Ready/InstanceManaged/GraphResolved/ResourcesReady all True.
- Service controller allocated `spec.clusterIP=10.86.192.70` and created Endpoints `dev/hello`.
- Custom Metrics API: POST `/apis/custom.metrics.k8s.io/v1beta1/namespaces/dev/services/hello/requests-per-second/12.5` → MetricValueList; GET returns the same (value "12.5", describedObject Service hello).

## Bugs found and fixed in this pass

1. `manifests/rgd/function.yaml` used `${schema.spec.name}` but the Function schema has no `name` field —instance names live in `metadata.name`. All 8 occurrences → `${schema.metadata.name}`. Without this the Deployment render failed (`No such key: name`) and no children were created.
2. `kro::reconciler::deletion_plan` built core-v1 child DELETE targets with group `"core"` (the `Target::key()` spelling) instead of empty → Service delete hit `/apis/core/v1/services` (404) so wave deletion stalled. Normalized `"core"` → `""` in `split_gvr_key`.
3. `kro::reconciler::child_patch` compared the entire stripped child against the desired template. Controller-added fields (Service `spec.clusterIP`) made every pass re-patch, so nodes never confirmed and the instance stayed `IN_PROGRESS` forever on a host with controllers. Now an intent SUBSET check: extra stored fields are fine (`json_subset`); only missing/differing desired fields patch. Added unit test.
4. (Earlier in this session) kro engine released the per-instance in-flight guard before reconcile → two workers could reconcile one instance concurrently (duplicate Creates, status races). Guard now held for the full pass with a pending set.
5. (Earlier) AppState gained a `metrics` field and broke 9 test initializers — added `AppState::new(conn, auth)`.

## Environment gaps on qreta-dev (not Function RGD bugs)

- No CRI-O socket (`/run/crio/crio.sock`), no `wisolate`, no `crio` binary → the kubelet CRI deploy arm cannot run pods; Deployment controller sees the Deployment but plans 0 ReplicaSets (no runtime to bind).
- The embedded gateway was removed from kapi (commit f3963bb); there is no gateway binary on dev and the config `[gateway]` section is now inert. Route table contract is verified via the HTTPRoute rows the gateway reads (typed gateway.networking.k8s.io/v1 HTTPRoute).
- Scale-to-zero not tested (excluded per owner).
- HPA CRD is not served (404 on /apis/autoscaling/v2/...horizontalpodautoscalers) — the RPS custom metric endpoint works but there is no HPA controller.
- Embedded controller watch reactivity on dev appears stale after boot (no new list reconciled log after the initial pass); the Function RGD itself reconciles correctly on fresh boot/restart. Worth a separate investigation.

## Local regression test

`crates/kro-kube/tests/function_integration.rs` — boots a kapi server, applies the real manifest, creates a Function instance, asserts the Service/Deployment/2 HTTPRoutes, native JSON command/args/env, status projection (ready/reason after a Deployment status patch), ACTIVE + conditions, and wave deletion. Passes.

## Next phase — WorkerJs / WorkerPy as pure RGDs

The plan: define WorkerJs and WorkerPy as ResourceGraphDefinitions built ON TOP of the Function RGD (instance kind Function with image=wisolate runtime + command/args/env for the JS/Python worker), so no custom controllers are needed for wisolate definitions. Prerequisites: CRI-O + wisolate on the dev node, a gateway binary/config for public routes, and a decision on whether Worker* RGDs should create Function instances (nested RGD) or directly emit the Service/Deployment/HTTPRoute graph.

## WorkerJs on wisolate — implemented + verified on qreta-dev (same session)

### Binaries copied to qreta-dev
- `wisolate` (musl static, `admin_notify,mimalloc`) → `/usr/local/bin/wisolate` (55 MB, `wisolate 0.1.0`).
- Running as `wisolate serve --config /etc/wisolate/wisolate.toml --k8s-controller` (root, log `/var/log/wisolate-dev.log`), listening `127.0.0.1:8787`.
- Config uses bucket `wisolate-dev` (fresh bucket created via S3 API so VersityGW wrote ACL metadata — reusing `wisolate2` 500s on `PUT /wisolate2/` because that bucket was mkdir-created with no ACL).
- GHCR pull token supplied via `$WISOLATE_REGISTRY_TOKEN` (file `/etc/wisolate/registry.env`, 600).

### Definitions (in this repo)
- `manifests/rgd/wisolate-function.yaml` — ONE shared Function instance `wisolate` → Service `wisolate` + Deployment `wisolate`, `args: [serve, --k8s-controller]`, `routes: []` (no per-app pods; no scale-to-zero).
- `manifests/rgd/workerjs.yaml` — WorkerJs RGD whose instances create:
  1. a native `Wisolate` row (`wisolate.qreta.io/v1alpha1`) with `oci`, `routes`, `env` — consumed by the shared node's embedded k8s controller;
  2. one `HTTPRoute` per declared route, backendRef → the shared `wisolate` Service port 80.
  - Instance kind plural is `workerjsapps` (group `kapi.qreta.io`) because kapi keys CRDs by plural across ALL groups and the native `workerjses.wisolate.qreta.io` CRD already owns that plural.
  - `readyWhen` uses `has(app.status) && has(app.status.imageDigest)`; the node writes `imageDigest` after a successful apply.
- Tests: `crates/kro/tests/workerjs_rgd.rs` (2), `crates/kro-kube/tests/workerjs_integration.rs` (1) — all pass.

### Live verification (qreta-dev)
- All 9 native wisolate CRDs registered via the CR surface (201 each).
- WorkerJs RGD accepted: `GraphAccepted/KindReady/ControllerReady` True; `workerjsapps.kapi.qreta.io` CRD registered.
- Shared Function `wisolate` → Service + Deployment created (200).
- WorkerJs instance `default/hello-js` (oci `ghcr.io/qretaio/games:latest`, route `hellojs.qreta.io`) → native Wisolate row + HTTPRoute created; HTTPRoute backendRef = `wisolate:80`.
- The wisolate node pulled the app OCI image, applied it, wrote `imageDigest`/`version` onto the Wisolate row; `/admin/status` shows app `hello-js` warm with the declared route.
- WorkerJs instance reached **ACTIVE** once the node published the child status.
- Note: the shared Deployment pods cannot run on dev (no CRI-O); the running node is a host process. The application image path used here was `games:latest` (a published app image), not the non-existent `worker-js:latest`.

