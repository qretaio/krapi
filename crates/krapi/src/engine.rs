//! THE ENGINE — the dynamic loop set. Unlike the fixed-target controllers,
//! the engine learns its surfaces at runtime: every accepted RGD
//! contributes an instance-kind pump and one pump per distinct child GVR.
//! The pieces:
//!
//! * **pumps** — one list → resync → watch loop per surface (the
//!   list→watch gap law, the 410 Gone heal, the jittered reconnect),
//!   forwarding events into one central channel,
//! * **the dispatcher** — owns the state (compiled graphs, caches,
//!   pumps), routes every event, enqueues instance reconciles,
//! * **workers** — drain the work queue and run one reconcile per
//!   instance, applying the plan through the face,
//! * **the beat** — re-enqueues not-ready instances on their backoff.

use crate::face::{Action, Face, FaceError, Listed, SharedFace, Target, WatchEvent};
use crate::graph::{CompiledGraph, EngineDefaults, child_targets, instance_target};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// The engine's tuning knobs.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub defaults: EngineDefaults,
    /// The reconcile worker count.
    pub workers: usize,
    /// The not-ready requeue beat.
    pub tick: Duration,
    /// The base not-ready backoff (capped exponential off this).
    pub backoff_base: Duration,
    /// The backoff cap.
    pub backoff_cap: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            defaults: EngineDefaults::default(),
            workers: 4,
            tick: Duration::from_secs(2),
            backoff_base: Duration::from_secs(2),
            backoff_cap: Duration::from_secs(60),
        }
    }
}

/// One instance's work key: the instance GVR + identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct InstKey {
    group: String,
    version: String,
    plural: String,
    namespace: String,
    name: String,
}

impl InstKey {
    fn of(graph: &CompiledGraph, obj: &Json) -> Self {
        Self {
            group: graph.instance_group.clone(),
            version: graph.instance_version.clone(),
            plural: graph.instance_plural.clone(),
            namespace: obj
                .pointer("/metadata/namespace")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
            name: obj
                .pointer("/metadata/name")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

/// The dispatcher's state — every mutable fact the engine holds.
#[derive(Default)]
struct State {
    /// The compiled graphs, keyed by RGD name.
    graphs: BTreeMap<String, CompiledGraph>,
    /// The RGD's instance CRD has been applied (don't re-create).
    crds_applied: BTreeSet<String>,
    /// The live pumps, keyed by target key.
    pumps: BTreeMap<String, tokio::task::AbortHandle>,
    /// The latest body per instance key.
    instances: BTreeMap<InstKey, Json>,
    /// The children cache: target key → (ns/name → body).
    children: BTreeMap<String, BTreeMap<String, Json>>,
    /// The instances sitting in not-ready backoff: key → (attempt, due).
    backoff: BTreeMap<InstKey, (u32, std::time::Instant)>,
    /// The keys queued or being reconciled (the in-flight guard).
    inflight: BTreeSet<InstKey>,
    /// Events that landed while a key was in flight. The worker drains
    /// these into a fresh pass after it releases the guard — no event is
    /// dropped, and no two reconciles of one instance overlap.
    pending: BTreeSet<InstKey>,
    /// The instance-kind pump key → RGD name (the reverse lookup).
    rgd_by_kind: BTreeMap<String, String>,
}

/// One event off any pump.
struct PumpEvent {
    target: Target,
    event: WatchEvent,
}

/// The messages workers send back after a reconcile.
enum WorkerMsg {
    /// Soft not-ready: schedule a backoff requeue.
    Backoff(InstKey),
}

/// The engine. Run it with [`Engine::run`] — it never returns short of
/// process end.
pub struct Engine {
    face: SharedFace,
    config: EngineConfig,
}

impl Engine {
    #[must_use]
    pub fn new(face: SharedFace, config: EngineConfig) -> Self {
        Self { face, config }
    }

    /// Run the engine to process end — every loop logs and continues.
    pub async fn run(&self) {
        let (pump_tx, mut pump_rx) = mpsc::channel::<PumpEvent>(1024);
        let (work_tx, work_rx) = mpsc::channel::<InstKey>(4096);
        let (worker_tx, mut worker_rx) = mpsc::channel::<WorkerMsg>(4096);

        let state = Arc::new(Mutex::new(State::default()));
        // THE IN-FLIGHT GUARD: one reconcile per instance at a time. An
        // event enqueues only when the key is not already queued or being
        // processed; the worker releases the key when it pops, so events
        // landing mid-reconcile re-enqueue and the next pass sees fresh
        // state. Without the guard, two workers race one instance — a
        // pruning pass interleaved with a creating pass.
        let face = Arc::new(self.face.clone());
        let config = Arc::new(self.config.clone());

        // The RGD pump is the one fixed surface.
        tokio::spawn(pump(
            "rgd".to_owned(),
            Arc::clone(&face),
            config.defaults.rgd_target(),
            pump_tx.clone(),
        ));

        // The workers share the one work receiver through an async
        // mutex (pop, release, reconcile).
        let work_rx = Arc::new(tokio::sync::Mutex::new(work_rx));
        let mut joins = Vec::new();
        for i in 0..self.config.workers {
            joins.push(tokio::spawn(worker(
                i,
                Arc::clone(&state),
                Arc::clone(&face),
                Arc::clone(&work_rx),
                work_tx.clone(),
                worker_tx.clone(),
            )));
        }
        drop(worker_tx);

        // The beat: re-enqueue backoff-due instances.
        let beat_state = Arc::clone(&state);
        let beat_work = work_tx.clone();
        let tick = self.config.tick;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let now = std::time::Instant::now();
                let due: Vec<InstKey> = {
                    let Ok(mut s) = beat_state.lock() else {
                        continue;
                    };
                    let due: Vec<InstKey> = s
                        .backoff
                        .iter()
                        .filter(|(_, (_, at))| *at <= now)
                        .map(|(k, _)| k.clone())
                        .collect();
                    for k in &due {
                        s.backoff.remove(k);
                    }
                    due
                };
                for k in due {
                    // The same guarded enqueue as the event path: a key
                    // still in flight becomes pending, not a second
                    // overlapping reconcile.
                    enqueue(&beat_state, &beat_work, &k);
                }
            }
        });
        // The dispatcher: every pump event routes here; worker outcomes
        // drain between events.
        loop {
            let Some(ev) = pump_rx.recv().await else {
                break;
            };
            dispatch(&state, &face, &config, &work_tx, &pump_tx, ev).await;
            while let Ok(msg) = worker_rx.try_recv() {
                match msg {
                    WorkerMsg::Backoff(key) => {
                        let Ok(mut s) = state.lock() else {
                            break;
                        };
                        let base = config.backoff_base;
                        let cap = config.backoff_cap;
                        let attempt = s.backoff.get(&key).map_or(1, |(a, _)| a + 1);
                        let delay = base.saturating_mul(attempt);
                        let delay = delay.min(cap);
                        s.backoff
                            .insert(key, (attempt, std::time::Instant::now() + delay));
                    }
                }
            }
        }
        drop(pump_tx);
        drop(work_tx);
        // The pumps die with the channel; abort whatever is left.
        if let Ok(s) = state.lock() {
            for h in s.pumps.values() {
                h.abort();
            }
        }
        for j in joins {
            let _ = j.await;
        }
    }
}

/// One surface's list → resync → watch loop (the gap law, the 410 heal,
/// the jittered reconnect).
async fn pump(name: String, face: Arc<SharedFace>, target: Target, tx: mpsc::Sender<PumpEvent>) {
    let jitter = reconnect_delay(&target);
    let mut resource_version: Option<String> = None;
    loop {
        // The CRD may still be establishing: a NotFound list is a soft
        // wait, retried on the reconnect beat.
        match face.list(&target).await {
            Ok(Listed {
                items,
                resource_version: list_rv,
            }) => {
                let count = items.len();
                tracing::info!(pump = %name, plural = %target.plural, count, "kro: list reconciled");
                for item in items {
                    let event = WatchEvent::Added(item);
                    if tx
                        .send(PumpEvent {
                            target: target.clone(),
                            event,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                resource_version = list_rv.or(resource_version);
            }
            Err(FaceError::NotFound(m)) => {
                tracing::info!(pump = %name, "kro: surface not established yet ({m}) — retrying");
                tokio::time::sleep(jitter).await;
                continue;
            }
            Err(e) => {
                tracing::warn!(pump = %name, "kro: list failed: {e}");
                tokio::time::sleep(jitter).await;
                continue;
            }
        }
        let mut rx = match face.watch(&target, resource_version.as_deref()).await {
            Ok(rx) => rx,
            Err(FaceError::Gone) => {
                tracing::info!(pump = %name, "kro: watch 410 — the version drops, re-list heals");
                resource_version = None;
                tokio::time::sleep(jitter).await;
                continue;
            }
            Err(e) => {
                tracing::warn!(pump = %name, "kro: watch failed ({e}) — retrying");
                tokio::time::sleep(jitter).await;
                continue;
            }
        };
        while let Some(event) = rx.recv().await {
            if let Some(rv) = event_resource_version(&event) {
                resource_version = Some(rv);
            }
            if matches!(event, WatchEvent::Bookmark(_)) {
                continue;
            }
            if let WatchEvent::Error(status) = &event {
                tracing::warn!(
                    pump = %name,
                    code = status.code,
                    "kro: watch errored in-stream — reconnecting"
                );
                if status.code == 410 {
                    resource_version = None;
                }
                break;
            }
            if tx
                .send(PumpEvent {
                    target: target.clone(),
                    event,
                })
                .await
                .is_err()
            {
                return;
            }
        }
        tracing::info!(pump = %name, "kro: watch stream ended — reconnecting");
        tokio::time::sleep(jitter).await;
    }
}

/// Pull an rv out of a watch event, when one is there.
fn event_resource_version(event: &WatchEvent) -> Option<String> {
    let body = match event {
        WatchEvent::Added(b) | WatchEvent::Modified(b) | WatchEvent::Deleted(b) => b,
        WatchEvent::Bookmark(b) => return Some(b.metadata.resource_version.clone()),
        WatchEvent::Error(_) => return None,
    };
    body.pointer("/metadata/resourceVersion")
        .and_then(Json::as_str)
        .map(str::to_owned)
}

/// The FNV-jittered reconnect (1s..2s, keyed by target — the herd
/// flattener).
fn reconnect_delay(target: &Target) -> Duration {
    let key = target.key();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    Duration::from_millis(1000 + (h % 1000))
}

/// Route one pump event through the state.
async fn dispatch(
    state: &Arc<Mutex<State>>,
    face: &Arc<SharedFace>,
    config: &Arc<EngineConfig>,
    work_tx: &mpsc::Sender<InstKey>,
    pump_tx: &mpsc::Sender<PumpEvent>,
    ev: PumpEvent,
) {
    let rgd_target = config.defaults.rgd_target();
    if ev.target == rgd_target {
        handle_rgd_event(state, face, config, pump_tx, &ev).await;
        return;
    }
    // An instance-kind event?
    let target_key = ev.target.key();
    let rgd_name = {
        let Ok(s) = state.lock() else {
            return;
        };
        s.rgd_by_kind.get(&target_key).cloned()
    };
    if let Some(rgd_name) = rgd_name {
        handle_instance_event(state, work_tx, &ev, &rgd_name);
        return;
    }
    handle_child_event(state, work_tx, &ev);
}

/// An RGD event: compile, ensure the instance CRD, (re)spawn pumps, or
/// tear down.
#[allow(clippy::too_many_lines)] // accept, refuse and teardown read best in one arm
async fn handle_rgd_event(
    state: &Arc<Mutex<State>>,
    face: &Arc<SharedFace>,
    config: &Arc<EngineConfig>,
    pump_tx: &mpsc::Sender<PumpEvent>,
    ev: &PumpEvent,
) {
    let now = now_rfc3339();
    let generation_of = |b: &Json| {
        b.pointer("/metadata/generation")
            .and_then(Json::as_u64)
            .unwrap_or(1)
    };
    // THE STATUS DIFF LAW: a status patch that would not change the
    // state or any condition's meaning is skipped — every patch echoes
    // a Modified event, and an unconditional patch feeds itself
    // forever. Only the meaning-bearing tuple compares; the
    // transition timestamps are bookkeeping, not meaning.
    let status_differs = |body: &Json, desired: &Json| -> bool {
        let meaning = |s: &Json| -> Json {
            json!({
                "state": s.get("state").cloned().unwrap_or(Json::Null),
                "conditions": s.get("conditions").and_then(Json::as_array).map_or_default(|cs| {
                    cs.iter().map(|c| json!({
                        "type": c.get("type").cloned().unwrap_or(Json::Null),
                        "status": c.get("status").cloned().unwrap_or(Json::Null),
                        "reason": c.get("reason").cloned().unwrap_or(Json::Null),
                        "message": c.get("message").cloned().unwrap_or(Json::Null),
                    })).collect::<Vec<_>>()
                }),
            })
        };
        meaning(body.get("status").unwrap_or(&Json::Null)) != meaning(desired)
    };
    match &ev.event {
        WatchEvent::Deleted(body) => {
            let name = obj_name(body);
            teardown_rgd(state, face, &name).await;
        }
        WatchEvent::Added(body) | WatchEvent::Modified(body) => {
            let refused = |message: String, _rgd_name: String, generation: u64| {
                json!({
                    "status": {
                        "state": "Inactive",
                        "conditions": [{
                            "type": "GraphAccepted",
                            "status": "False",
                            "reason": "InvalidResourceGraph",
                            "message": message,
                            "lastTransitionTime": now,
                            "observedGeneration": generation,
                        }],
                    }
                })
            };
            let parsed = crate::api::parse(body, &config.defaults.instance_group);
            let rgd = match parsed {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(rgd = %obj_name(body), "kro: RGD refused: {e}");
                    let desired = refused(e, obj_name(body), generation_of(body));
                    if let Some(status) = desired.get("status")
                        && status_differs(body, status)
                        && let Err(pe) = face
                            .apply(&Action::PatchStatus {
                                target: ev.target.clone(),
                                namespace: None,
                                name: obj_name(body),
                                body: desired,
                            })
                            .await
                    {
                        tracing::warn!("kro: the RGD refusal status failed to persist: {pe}");
                    }
                    return;
                }
            };
            let graph = match crate::graph::compile(&rgd, &config.defaults) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!(rgd = %rgd.name, "kro: RGD compile failed: {e}");
                    let desired = refused(e.to_string(), rgd.name.clone(), rgd.generation);
                    if let Some(status) = desired.get("status")
                        && status_differs(body, status)
                        && let Err(pe) = face
                            .apply(&Action::PatchStatus {
                                target: ev.target.clone(),
                                namespace: None,
                                name: rgd.name.clone(),
                                body: desired,
                            })
                            .await
                    {
                        tracing::warn!(
                            "kro: the RGD compile-failure status failed to persist: {pe}"
                        );
                    }
                    return;
                }
            };

            let inst_target = instance_target(&graph);
            let instance_pump_key = inst_target.key();
            let (first_accept, unchanged) = {
                let Ok(mut s) = state.lock() else {
                    return;
                };
                let unchanged = s
                    .graphs
                    .get(&rgd.name)
                    .is_some_and(|g| g.spec_hash == graph.spec_hash)
                    && s.pumps.contains_key(&instance_pump_key);
                let first = !s.crds_applied.contains(&rgd.name);
                s.graphs.insert(rgd.name.clone(), graph.clone());
                s.rgd_by_kind
                    .insert(instance_pump_key.clone(), rgd.name.clone());
                (first, unchanged)
            };
            if unchanged {
                return;
            }

            // Accept: the status lands first (the echo it triggers is
            // absorbed by the unchanged gate above), then the CRD, then
            // the pumps.
            let topological_order: Vec<Json> =
                graph.topological_order.iter().map(|id| json!(id)).collect();
            if let Err(pe) = face
                .apply(&Action::PatchStatus {
                    target: ev.target.clone(),
                    namespace: None,
                    name: rgd.name.clone(),
                    body: json!({
                        "status": {
                            "state": "Active",
                            "topologicalOrder": topological_order,
                            "conditions": [
                                {"type": "GraphAccepted", "status": "True", "reason": "Valid",
                                 "message": "graph accepted", "lastTransitionTime": now,
                                 "observedGeneration": rgd.generation},
                                {"type": "KindReady", "status": "True", "reason": "CRDApplied",
                                 "message": "instance CRD applied", "lastTransitionTime": now,
                                 "observedGeneration": rgd.generation},
                                {"type": "ControllerReady", "status": "True", "reason": "WatchStarted",
                                 "message": "instance reconciler watching", "lastTransitionTime": now,
                                 "observedGeneration": rgd.generation},
                            ],
                        }
                    }),
                })
                .await
            {
                tracing::warn!("kro: the RGD accept status failed to persist: {pe}");
            }

            if first_accept {
                if let Err(e) = face
                    .apply(&Action::Create {
                        target: Target::crds(),
                        body: graph.instance_crd.clone(),
                    })
                    .await
                {
                    tracing::warn!(rgd = %rgd.name, "kro: the instance CRD create failed: {e}");
                }
                if let Ok(mut s) = state.lock() {
                    s.crds_applied.insert(rgd.name.clone());
                }
            }

            spawn_pumps(state, face, pump_tx, &graph);
            tracing::info!(
                rgd = %rgd.name,
                kind = %graph.instance_kind,
                nodes = graph.nodes.len(),
                "kro: RGD accepted"
            );
        }
        WatchEvent::Bookmark(_) | WatchEvent::Error(_) => {}
    }
}

/// Spawn (or keep) the pumps one accepted RGD needs.
fn spawn_pumps(
    state: &Arc<Mutex<State>>,
    face: &Arc<SharedFace>,
    tx: &mpsc::Sender<PumpEvent>,
    graph: &CompiledGraph,
) {
    let wanted: BTreeMap<String, Target> = {
        let mut w = BTreeMap::new();
        let inst_target = instance_target(graph);
        w.insert(inst_target.key(), inst_target);
        for t in child_targets(graph) {
            w.insert(t.key(), t);
        }
        w
    };
    let Ok(mut s) = state.lock() else {
        return;
    };
    for (key, target) in wanted {
        if s.pumps.contains_key(&key) {
            continue;
        }
        let handle = tokio::spawn(pump(key.clone(), Arc::clone(face), target, tx.clone()));
        s.pumps.insert(key, handle.abort_handle());
    }
}

/// An instance event: cache the body, enqueue the reconcile.
fn handle_instance_event(
    state: &Arc<Mutex<State>>,
    work_tx: &mpsc::Sender<InstKey>,
    ev: &PumpEvent,
    rgd_name: &str,
) {
    let body = match &ev.event {
        WatchEvent::Added(b) | WatchEvent::Modified(b) | WatchEvent::Deleted(b) => b.clone(),
        WatchEvent::Bookmark(_) | WatchEvent::Error(_) => return,
    };
    let Ok(mut s) = state.lock() else {
        return;
    };
    let Some(graph) = s.graphs.get(rgd_name) else {
        return;
    };
    let key = InstKey::of(graph, &body);
    if matches!(ev.event, WatchEvent::Deleted(_)) {
        s.instances.remove(&key);
        // A queued pass for a now-deleted instance is stale. Drop its
        // guard instead of leaving it held forever (no future event
        // would ever re-enqueue it).
        s.pending.remove(&key);
        s.backoff.remove(&key);
        return;
    }
    s.instances.insert(key.clone(), body);
    drop(s);
    enqueue(state, work_tx, &key);
}

/// A child event: cache, resolve the owner, enqueue the owner.
fn handle_child_event(state: &Arc<Mutex<State>>, work_tx: &mpsc::Sender<InstKey>, ev: &PumpEvent) {
    let body = match &ev.event {
        WatchEvent::Added(b) | WatchEvent::Modified(b) | WatchEvent::Deleted(b) => b.clone(),
        WatchEvent::Bookmark(_) | WatchEvent::Error(_) => return,
    };
    let target_key = ev.target.key();
    let obj_key = obj_cache_key(&body);
    let owner = crate::labels::owning_instance(&body);
    let Ok(mut s) = state.lock() else {
        return;
    };
    let rows = s.children.entry(target_key).or_default();
    match &ev.event {
        WatchEvent::Deleted(_) => {
            rows.remove(&obj_key);
        }
        _ => {
            rows.insert(obj_key, body.clone());
        }
    }
    let Some((ns, name)) = owner else {
        return;
    };
    // The owner labels name the instance kind — find the RGD whose
    // instance kind matches and enqueue that instance.
    let label_kind = body
        .pointer("/metadata/labels/kro.rs~1instance-kind")
        .and_then(Json::as_str)
        .unwrap_or_default();
    for graph in s.graphs.values() {
        if graph.instance_kind != label_kind {
            continue;
        }
        let key = InstKey {
            group: graph.instance_group.clone(),
            version: graph.instance_version.clone(),
            plural: graph.instance_plural.clone(),
            namespace: ns.unwrap_or_default(),
            name,
        };
        if s.instances.contains_key(&key) {
            drop(s);
            enqueue(state, work_tx, &key);
        }
        return;
    }
}

/// Tear one RGD down: children first (reverse-wave order), then the CRD,
/// then the pumps and state.
async fn teardown_rgd(state: &Arc<Mutex<State>>, face: &Arc<SharedFace>, name: &str) {
    let (graph, inst_key, child_keys) = {
        let Ok(s) = state.lock() else {
            return;
        };
        let Some(graph) = s.graphs.get(name).cloned() else {
            return;
        };
        let inst_key = s
            .rgd_by_kind
            .iter()
            .find(|(_, r)| r.as_str() == name)
            .map(|(k, _)| k.clone());
        let child_keys: Vec<String> = child_targets(&graph)
            .iter()
            .map(crate::face::Target::key)
            .collect();
        (graph, inst_key, child_keys)
    };
    // Every instance's children, reverse-wave order.
    let mut deletes: Vec<Action> = Vec::new();
    {
        let Ok(s) = state.lock() else {
            return;
        };
        for body in s.instances.values() {
            if body.get("kind").and_then(Json::as_str) != Some(graph.instance_kind.as_str()) {
                continue;
            }
            for wave in graph.deletion_waves.iter().rev() {
                for id in wave {
                    let Some(node) = graph.nodes.iter().find(|n| &n.id == id) else {
                        continue;
                    };
                    let gvr = crate::reconciler::gvr_key(node);
                    if let Some(rows) = s.children.get(&gvr) {
                        for row in rows.values() {
                            if crate::reconciler::owned_by_instance(row, body) {
                                deletes.push(Action::Delete {
                                    target: Target::cr(
                                        node.group.as_deref().unwrap_or(""),
                                        &node.version,
                                        &node.plural,
                                        None,
                                    ),
                                    namespace: row
                                        .pointer("/metadata/namespace")
                                        .and_then(Json::as_str)
                                        .map(str::to_owned),
                                    name: row
                                        .pointer("/metadata/name")
                                        .and_then(Json::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    for d in deletes {
        let _ = face.apply(&d).await;
    }
    // The instance CRD goes last (kapi cascades its rows with it; real
    // clusters delete the CRs too).
    let _ = face
        .apply(&Action::Delete {
            target: Target::crds(),
            namespace: None,
            name: format!("{}.{}", graph.instance_plural, graph.instance_group),
        })
        .await;
    let Ok(mut s) = state.lock() else {
        return;
    };
    s.graphs.remove(name);
    if let Some(k) = inst_key {
        s.rgd_by_kind.remove(&k);
        if let Some(h) = s.pumps.remove(&k) {
            h.abort();
        }
    }
    for k in child_keys {
        if let Some(h) = s.pumps.remove(&k) {
            h.abort();
        }
        s.children.remove(&k);
    }
    s.crds_applied.remove(name);
}

/// One worker: pop a key, snapshot, reconcile, apply.
async fn worker(
    id: usize,
    state: Arc<Mutex<State>>,
    face: Arc<SharedFace>,
    work_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<InstKey>>>,
    work_tx: mpsc::Sender<InstKey>,
    worker_tx: mpsc::Sender<WorkerMsg>,
) {
    while let Some(key) = {
        let mut rx = work_rx.lock().await;
        rx.recv().await
    } {
        // THE SERIALIZATION LAW: the in-flight guard is held for the
        // WHOLE pass (snapshot -> plan -> applies). Releasing it early let
        // a second worker reconcile the same instance concurrently —
        // both saw no child cached and both Create'd (one "already
        // exists"), or a status patch raced the row reload ("not found").
        // Events landing mid-pass go to pending and are re-enqueued
        // below, so no event is lost either.
        // The snapshot: the graph + instance body + this instance's owned
        // children plus the whole cache (external refs read without owner
        // labels).
        let snapshot = {
            let Ok(mut s) = state.lock() else {
                // A poisoned lock leaks the guard; the engine is
                // effectively dead at this point, but drop it so a
                // later lock cannot deadlock on a stale reservation.
                continue;
            };
            let Some(graph) = s.graphs.values().find(|g| {
                g.instance_group == key.group
                    && g.instance_version == key.version
                    && g.instance_plural == key.plural
            }) else {
                // A deleted RGD tore the graph down: release the guard
                // so a future RGD with the same kind can reconcile.
                s.inflight.remove(&key);
                continue;
            };
            let graph = graph.clone();
            let Some(instance) = s.instances.get(&key).cloned() else {
                s.inflight.remove(&key);
                s.pending.remove(&key);
                continue;
            };
            let mut children: crate::reconciler::Children = BTreeMap::new();
            for node in &graph.nodes {
                let gvr = crate::reconciler::gvr_key(node);
                if let Some(rows) = s.children.get(&gvr) {
                    children.insert(
                        gvr,
                        rows.values()
                            .filter(|r| crate::reconciler::owned_by_instance(r, &instance))
                            .cloned()
                            .collect(),
                    );
                }
            }
            // External refs ride the whole cache (they carry no owner
            // labels, so the owned filter would drop them). This includes
            // INSTANCE bodies: an externalRef may name a dynamic kind
            // whose pump is an instance pump (routed to `instances`, not
            // `children`) — e.g. WorkerJs referencing the shared Function
            // instance. Without this the ref never resolves.
            for (gvr, rows) in &s.children {
                let entry = children.entry(gvr.clone()).or_default();
                for r in rows.values() {
                    if !entry.contains(r) {
                        entry.push(r.clone());
                    }
                }
            }
            for (ikey, body) in &s.instances {
                let gvr = format!("{}/{}/{}@*", ikey.group, ikey.version, ikey.plural);
                children.entry(gvr).or_default().push(body.clone());
            }
            (graph, instance, children)
        };
        let (graph, instance, children) = snapshot;
        let now = now_rfc3339();
        let plan = crate::reconciler::reconcile(&graph, &instance, &children, &now);
        for action in &plan.actions {
            if let Err(e) = face.apply(action).await {
                // a delete racing the row's own purge is a no-op, not a
                // failure — the 404 means the drain already completed
                let gone = matches!(&e, crate::face::FaceError::NotFound(_))
                    && matches!(action, Action::Delete { .. });
                if gone {
                    tracing::debug!(worker = id, instance = %key.name, "kro: delete raced the purge — no-op");
                } else {
                    tracing::warn!(
                        worker = id,
                        verb = action.verb(),
                        instance = %key.name,
                        "kro: apply failed: {e}"
                    );
                }
            }
        }
        if plan.requeue {
            let _ = worker_tx.try_send(WorkerMsg::Backoff(key.clone()));
        }
        // Release the guard, then promote one pending event (if any)
        // into a fresh pass. Multiple events collapse to one — the next
        // pass re-reads the latest cached state, which is all a
        // level-triggered reconciler needs.
        let again = {
            let Ok(mut s) = state.lock() else {
                continue;
            };
            s.inflight.remove(&key);
            s.pending.remove(&key)
        };
        if again {
            // Reserve the guard before the blocking send so the next
            // pass is serialized too (same rule as enqueue()).
            let reserved = {
                let Ok(mut s) = state.lock() else {
                    break;
                };
                s.inflight.insert(key.clone())
            };
            if reserved && work_tx.send(key.clone()).await.is_err() {
                if let Ok(mut s) = state.lock() {
                    s.inflight.remove(&key);
                }
                break;
            }
        }
    }
}

/// Enqueue one instance reconcile through the in-flight guard.
fn enqueue(state: &Arc<Mutex<State>>, work_tx: &mpsc::Sender<InstKey>, key: &InstKey) {
    // The guard rides the state mutex (short critical sections — the
    // queue send happens outside). A key already queued or being
    // reconciled records a pending pass instead of starting a second
    // overlapping one.
    let fresh = {
        let Ok(mut s) = state.lock() else {
            return;
        };
        if s.inflight.contains(key) {
            s.pending.insert(key.clone());
            false
        } else {
            s.inflight.insert(key.clone())
        }
    };
    if !fresh {
        return;
    }
    if work_tx.try_send(key.clone()).is_err()
        && let Ok(mut s) = state.lock()
    {
        s.inflight.remove(key);
    }
}

/// The object's cache key (namespace/name).
fn obj_cache_key(obj: &Json) -> String {
    format!(
        "{}/{}",
        obj.pointer("/metadata/namespace")
            .and_then(Json::as_str)
            .unwrap_or_default(),
        obj.pointer("/metadata/name")
            .and_then(Json::as_str)
            .unwrap_or_default()
    )
}

fn obj_name(obj: &Json) -> String {
    obj.pointer("/metadata/name")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The RFC-3339 stamp (the status timestamps ride jiff).
#[must_use]
pub fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}
