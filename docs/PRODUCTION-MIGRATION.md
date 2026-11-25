# Function / WorkerJs production migration plan (staged)

## Current state (audit 2026-11-23)

- **Dev**: new model live and verified — kapi binary with kro fixes, RGDs `function` + `workerjs` Active, one shared Function instance `wisolate` (Service+Deployment, ACTIVE), WorkerJs instance `routeonly` (ACTIVE) whose only child is an HTTPRoute to `wisolate:80`. A host-process wisolate node serves the app. No CRI-O on dev (pods can't run).
- **Production**: old model — kapi (Nov 21 binary) with RGD CRD present but NO RGDs/Function/WorkerJs instances; standalone `io.qreta.wisolate.service` on `0.0.0.0:8789` (S3-backed, no `--k8s-controller`); separate `gateway-new` on 8787. CRI-O IS running. Gateway currently 500s for all app hosts because GHCR asset pulls fail (`error sending request` on `ghcr.io/...`) and version objects are missing — **no healthy baseline**.

## Target model

- One operator-managed shared Function instance `wisolate` → its Service + Deployment (the one wisolate node, `wisolate serve --k8s-controller`, S3 config, registry token).
- Native Wisolate app rows come from the Function instance (operator-managed), NOT from WorkerJs.
- Each WorkerJs instance reuses the shared Function (read-only externalRef) and adds only HTTPRoutes whose backendRef is the shared `wisolate` Service.

## Staged plan (no traffic cutover until explicitly approved)

1. **Fix/understand production 500s first** — the gateway cannot pull assets from GHCR; migration won't be verifiable until that is resolved (token/egress).
2. **Build artifacts locally** — current kapi musl binary (already built), wisolate musl binary (built), the three manifests (`function.yaml`, `wisolate-function.yaml`, `workerjs.yaml`).
3. **Stage on production, no cutover**:
   - Back up `/var/lib/kapi/kapi.db` and the gateway/wisolate configs.
   - Install the new kapi binary; keep the old one as `kapi-old` for rollback. Restart kapi (RGD CRD already exists).
   - Start a SECOND wisolate node on a different internal port, e.g. `8790`, with its own config/bucket/registry token and `--k8s-controller`. Do NOT stop the existing 8789 node or change the gateway.
   - Apply the `function` RGD; apply the shared Function instance `wisolate` with `routes: []` and a container port/targetPort of 8790 (or keep container 8789 and map Service to the new node). Verify the Deployment/SERVICE/Endpoints rows appear.
   - Apply the `workerjs` RGD; create one test WorkerJs instance with a test hostname pointing at the shared Service. Verify the HTTPRoute row appears and the RGD/instance reach ACTIVE.
4. **Verify** — point a temporary Host header / local curl at the gateway path that resolves the new route and confirm it reaches the 8790 node (`/admin/status` shows the app).
5. **Final cutover (separate approval)** — flip production HTTPRoutes' backendRef / gateway node mapping from the old 8789 node to the new 8790 one (or swap ports), then decommission the old standalone wisolate and route the real app hostnames into WorkerJs instances.
6. **Rollback** — restore backed-up DB + configs, restore old binaries, restart services; the old standalone node stays untouched until cutover.

## Risks / open items

- Production GHCR pull failures (500s) must be fixed first or there is no clean baseline.
- CRI-O is running on production but the Deployment controller previously planned 0 ReplicaSets on dev — verify on production before relying on pods.
- The gateway resolves backends via node addresses / `[gateway.nodes]`, not Service ClusterIPs; the exact port mapping must be confirmed before cutover.
- WorkerJs instance plural is `workerjsapps` (kapi keys CRDs by plural across groups; `workerjses` is taken by the native CRD).

