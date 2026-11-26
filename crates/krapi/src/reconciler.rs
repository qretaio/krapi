//! THE INSTANCE RECONCILER — one instance, one pass, one plan. The pass
//! walks the graph's topological order against a snapshot (the instance
//! body + the children the pumps cached), evaluates every node's
//! expressions, and returns the actions the engine applies. It holds no
//! locks and performs no I/O: everything it needs rides the snapshot,
//! everything it changes rides the plan.
//!
//! The states it can land in:
//! * [`PlanState::Active`] — every node applied and ready,
//! * [`PlanState::InProgress`] — a node is gated (a dependency not ready,
//!   an external ref absent, a `readyWhen` unsatisfied) or an expression
//!   is waiting on data that does not exist yet — soft, retried on the
//!   beat,
//! * [`PlanState::Error`] — a hard refusal, recorded in status,
//! * [`PlanState::Deleting`] — the instance is terminating; the plan
//!   deletes children wave by wave and drops the finalizer last.

use crate::cel::{self, Scope};
use crate::conditions::{self, Condition, Status as CondStatus};
use crate::expression::Seg;
use crate::graph::CompiledGraph;
use crate::labels;
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

/// The instance's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanState {
    InProgress,
    Active,
    Deleting,
    Error,
}

impl PlanState {
    /// The wire spelling (`status.state`).
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            PlanState::InProgress => "IN_PROGRESS",
            PlanState::Active => "ACTIVE",
            PlanState::Deleting => "DELETING",
            PlanState::Error => "ERROR",
        }
    }
}

/// One reconcile pass's answer.
#[derive(Debug, Default)]
pub struct Plan {
    pub actions: Vec<crate::face::Action>,
    pub state: Option<PlanState>,
    /// Soft not-ready: re-enqueue with a backoff (the beat serves it).
    pub requeue: bool,
}

/// The children snapshot: every child the pumps cached for this
/// instance's RGD, keyed by GVR identity (`group/version/plural`).
pub type Children = BTreeMap<String, Vec<Json>>;

/// JSON-pointer escape for a label key (dots are safe, `/` is not).
const NODE_ID_POINTER: &str = "/metadata/labels/kro.run~1node-id";
const INSTANCE_UID_POINTER: &str = "/metadata/labels/kro.run~1instance-uid";
const INSTANCE_NAME_POINTER: &str = "/metadata/labels/kro.run~1instance-name";
const INSTANCE_NS_POINTER: &str = "/metadata/labels/kro.run~1instance-namespace";

/// Run one reconcile pass.
#[allow(clippy::too_many_lines)] // the walk is the law; splitting it scatters the order
/// One node's walk outcome: Ready gates dependents; Applied gates them
/// too (the submission is trusted until the event says otherwise);
/// Waiting holds the message; Skipped is includeWhen's false.
#[derive(Debug, Clone, PartialEq)]
enum Verdict {
    Ready,
    Applied,
    Waiting(String),
    Skipped(String),
}

#[allow(clippy::too_many_lines)] // the walk, the prune and the projection read best in one law
pub fn reconcile(graph: &CompiledGraph, instance: &Json, children: &Children, now: &str) -> Plan {
    let meta_ns = instance
        .pointer("/metadata/namespace")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned();
    let meta_name = instance
        .pointer("/metadata/name")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned();
    let generation = instance
        .pointer("/metadata/generation")
        .and_then(Json::as_u64)
        .unwrap_or(1);
    let target = crate::graph::instance_target(graph);

    if instance
        .pointer("/metadata/deletionTimestamp")
        .is_some_and(|v| !v.is_null())
    {
        return deletion_plan(
            graph, instance, children, &target, &meta_ns, &meta_name, now,
        );
    }

    let mut plan = Plan::default();

    // ---- the scope -----------------------------------------------------
    let mut scope = Scope::new();
    scope.insert(
        "schema".to_owned(),
        json!({
            "apiVersion": format!("{}/{}", graph.instance_group, graph.instance_version),
            "kind": graph.instance_kind,
            "metadata": instance.get("metadata").cloned().unwrap_or(json!({})),
            "spec": instance.get("spec").cloned().unwrap_or(json!({})),
        }),
    );

    let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
    let mut applied_names: BTreeMap<String, Vec<(Option<String>, String)>> = BTreeMap::new();
    let hard_error: Option<String> = None;
    let mut any_soft = false;
    let mut any_unconfirmed = false;
    let mut wait_message = String::new();

    if let Some(a) = ensure_managed(graph, instance, &target, &meta_ns, &meta_name) {
        plan.actions.push(a);
    }

    for id in &graph.topological_order {
        let Some(node) = graph.nodes.iter().find(|n| &n.id == id) else {
            continue;
        };
        // A gated dependency stops the node here.
        if let Some(gate) = node
            .dependencies
            .iter()
            .find_map(|dep| match verdicts.get(dep) {
                Some(Verdict::Ready) => None,
                Some(Verdict::Applied) => {
                    Some(format!("waiting for dependency {dep} to be observed"))
                }
                Some(Verdict::Waiting(m)) => Some(format!("waiting for dependency {dep}: {m}")),
                Some(Verdict::Skipped(m)) => Some(format!("dependency {dep} skipped: {m}")),
                None => Some(format!("dependency {dep} has no verdict")),
            })
        {
            verdicts.insert(id.clone(), Verdict::Waiting(gate.clone()));
            any_soft = true;
            wait_message = gate;
            continue;
        }
        // includeWhen: false skips the node (contagious through the
        // verdicts above); a failed evaluation is soft.
        match eval_conditions(&node.include_when, &scope) {
            Ok(false) => {
                verdicts.insert(
                    id.clone(),
                    Verdict::Skipped("includeWhen is false".to_owned()),
                );
                continue;
            }
            Ok(true) => {}
            Err(e) => {
                let m = format!("resource {id}: includeWhen: {e}");
                verdicts.insert(id.clone(), Verdict::Waiting(m.clone()));
                any_soft = true;
                wait_message = m;
                continue;
            }
        }
        let items = match expand_for_each(node, &scope) {
            Ok(items) => items,
            Err(e) => {
                let m = format!("resource {id}: forEach: {e}");
                verdicts.insert(id.clone(), Verdict::Waiting(m.clone()));
                any_soft = true;
                wait_message = m;
                continue;
            }
        };

        let mut node_applied: Vec<(Option<String>, String)> = Vec::new();
        let mut node_waiting: Option<String> = None;
        let mut node_observed: Option<Json> = None;
        let mut node_confirmed = true;

        for item in &items {
            let mut item_scope = scope.clone();
            for (k, v) in &item.bindings {
                item_scope.insert(k.clone(), v.clone());
            }
            if !node.for_each.is_empty()
                && let Some(first) = item.bindings.values().next()
            {
                item_scope.insert("each".to_owned(), first.clone());
            }

            let rendered = match render_value(&node.template, &item_scope) {
                Ok(Some(o)) => o,
                Ok(None) => {
                    node_waiting = Some("expression omitted the template".to_owned());
                    break;
                }
                // a render failure is data-not-ready, not a hard error —
                // a missing upstream field is the gating case
                Err(e) => {
                    node_waiting = Some(format!("render: {e}"));
                    break;
                }
            };
            let name = rendered
                .pointer("/metadata/name")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned();
            if name.is_empty() {
                node_waiting =
                    Some("metadata.name rendered empty (identity data not ready?)".to_owned());
                break;
            }
            let ns = rendered
                .pointer("/metadata/namespace")
                .and_then(Json::as_str)
                .map(str::to_owned)
                .or_else(|| (!meta_ns.is_empty()).then(|| meta_ns.clone()));

            // External refs only read.
            if node.external {
                let found = children.get(&gvr_key(node)).and_then(|rows| {
                    rows.iter()
                        .find(|r| same_object(r, ns.as_deref(), &name))
                        .cloned()
                });
                match found {
                    Some(current) => {
                        node_observed = Some(current.clone());
                    }
                    None => {
                        node_waiting = Some(format!(
                            "external ref {} {}/{} not found",
                            node.kind,
                            ns.as_deref().unwrap_or(""),
                            name
                        ));
                    }
                }
                if node_waiting.is_some() {
                    break;
                }
                node_applied.push((ns, name));
                continue;
            }

            // Apply: create, or patch when the owned fields differ.
            let gvr = gvr_key(node);
            let current = children.get(&gvr).and_then(|rows| {
                rows.iter()
                    .find(|r| same_object(r, ns.as_deref(), &name))
                    .cloned()
            });
            let desired = stamp_child(node, rendered, &meta_ns, instance, graph);
            match current {
                None => {
                    plan.actions.push(crate::face::Action::Create {
                        target: child_target(node, ns.as_deref()),
                        body: desired.clone(),
                    });
                    // The scope carries the desired shape until the
                    // create's event lands — spec references resolve
                    // immediately; status references wait for the event.
                    node_observed = Some(desired.clone());
                    node_confirmed = false;
                }
                Some(current) => {
                    if let Some(patch) = child_patch(&current, &desired) {
                        plan.actions.push(crate::face::Action::Patch {
                            target: child_target(node, ns.as_deref()),
                            namespace: ns.clone(),
                            name: name.clone(),
                            body: patch.clone(),
                        });
                        node_observed = Some(merge(&current, &patch));
                        node_confirmed = false;
                    } else {
                        node_observed = Some(current);
                    }
                }
            }
            node_applied.push((ns, name));
        }

        // The observed object publishes into the outer scope FIRST — the
        // readiness verdict reads it, and so does the next node.
        if let Some(o) = node_observed {
            scope.insert(id.clone(), o);
        }
        if let Some(w) = node_waiting {
            let m = format!("resource {id}: {w}");
            verdicts.insert(id.clone(), Verdict::Waiting(m.clone()));
            any_soft = true;
            wait_message = m;
        } else if !node_confirmed {
            // submitted this pass: the confirm event (and the readiness
            // verdict) arrives with the child's watch event
            verdicts.insert(id.clone(), Verdict::Applied);
            any_unconfirmed = true;
        } else {
            match eval_ready(node, item_scopes(&items, &scope)) {
                Ok(true) => {
                    verdicts.insert(id.clone(), Verdict::Ready);
                }
                Ok(false) => {
                    let m = format!("resource {id}: readyWhen not satisfied");
                    verdicts.insert(id.clone(), Verdict::Waiting(m.clone()));
                    any_soft = true;
                    wait_message = m;
                }
                Err(e) => {
                    let m = format!("resource {id}: readyWhen: {e}");
                    verdicts.insert(id.clone(), Verdict::Waiting(m.clone()));
                    any_soft = true;
                    wait_message = m;
                }
            }
        }
        // (the debug line rides below, reading what the verdict left)
        if !node_applied.is_empty() {
            applied_names.insert(id.clone(), node_applied);
        }
    }

    // ---- orphan pruning: children the graph no longer applies ----------
    // THE KEPT UNION: several nodes may share one GVR (two Widgets in
    // one graph), and every row in that GVR's cache is visible to each
    // of their scans — a row is an orphan only when NO node claims it,
    // so the per-node applied names union first, per GVR.
    let mut claimed: BTreeMap<String, Vec<(Option<String>, String)>> = BTreeMap::new();
    for node in &graph.nodes {
        if node.external {
            continue;
        }
        if let Some(names) = applied_names.get(&node.id) {
            claimed
                .entry(gvr_key(node))
                .or_default()
                .extend(names.iter().cloned());
        }
    }
    let fully_resolved = !any_soft
        && hard_error.is_none()
        && !verdicts
            .values()
            .any(|v| matches!(v, Verdict::Applied | Verdict::Waiting(_)));
    if fully_resolved {
        for node in &graph.nodes {
            if node.external {
                continue;
            }
            let kept = claimed.get(&gvr_key(node)).cloned().unwrap_or_default();
            if let Some(rows) = children.get(&gvr_key(node)) {
                for row in rows {
                    let name = row
                        .pointer("/metadata/name")
                        .and_then(Json::as_str)
                        .unwrap_or_default();
                    let ns = row
                        .pointer("/metadata/namespace")
                        .and_then(Json::as_str)
                        .map(str::to_owned);
                    if name.is_empty() {
                        continue;
                    }
                    if !kept
                        .iter()
                        .any(|(kns, kn)| kn == name && kns.as_deref() == ns.as_deref())
                        && owned_by_instance(row, instance)
                    {
                        plan.actions.push(crate::face::Action::Delete {
                            target: child_target(node, ns.as_deref()),
                            namespace: ns,
                            name: name.to_owned(),
                        });
                    }
                }
            }
        }
    }

    // ---- status projection ---------------------------------------------
    let wire_conditions = instance
        .pointer("/status/conditions")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let prev = conditions::from_wire(&wire_conditions);
    let mut set = conditions::ConditionSet::for_object(&prev, generation);
    set.set(
        conditions::INSTANCE_MANAGED,
        CondStatus::True,
        "Managed",
        "instance is managed with finalizer and labels",
    );
    set.set(
        conditions::GRAPH_RESOLVED,
        CondStatus::True,
        "Resolved",
        "graph compiled and resources resolved",
    );
    let mut state = PlanState::InProgress;
    if let Some(e) = &hard_error {
        set.set(
            conditions::RESOURCES_READY,
            CondStatus::False,
            "NotReady",
            e,
        );
        state = PlanState::Error;
    } else if any_soft {
        set.set(
            conditions::RESOURCES_READY,
            CondStatus::False,
            "NotReady",
            &wait_message,
        );
    } else if any_unconfirmed {
        set.set(
            conditions::RESOURCES_READY,
            CondStatus::False,
            "Applying",
            "resources submitted this pass, awaiting the confirm events",
        );
    } else {
        set.set(
            conditions::RESOURCES_READY,
            CondStatus::True,
            "AllResourcesReady",
            "all resources are created and ready",
        );
        state = PlanState::Active;
    }
    // The root folds its dependents — a Ready=True on the wire means
    // every dependent is True.
    set.fold_root(&[
        conditions::INSTANCE_MANAGED,
        conditions::GRAPH_RESOLVED,
        conditions::RESOURCES_READY,
    ]);
    if set.root_ready() {
        state = PlanState::Active;
    }

    let mut status_body = json!({
        "state": state.wire(),
        "conditions": conditions::to_wire(&set.stamped(&prev, now)),
    });

    // Author status fields: fresh-status law — a field appears when its
    // expression resolves and disappears when its upstream does. The
    // STATE follows the conditions, never the projections: a field
    // waiting on upstream data does not demote an otherwise-ready
    // instance.
    let mut fields = serde_json::Map::new();
    for (path, expr) in &graph.instance_status.fields {
        if let Ok(cel::Rendered::Value(v)) = cel::eval(expr, &scope) {
            insert_at(&mut fields, path, v);
        }
    }
    for (k, v) in fields {
        status_body[k] = v;
    }

    // Author conditions block (when the RGD declares one): the evaluated
    // conditions lead, the built-ins follow.
    if !graph.instance_status.conditions.is_empty() {
        let mut author = Vec::new();
        let mut complete = true;
        for expr in &graph.instance_status.conditions {
            match cel::eval(expr, &scope) {
                Ok(cel::Rendered::Value(Json::Array(items))) => {
                    for item in items {
                        if let Some(c) = author_condition(&item, generation, now) {
                            author.push(c);
                        }
                    }
                }
                Ok(cel::Rendered::Value(item)) => {
                    if let Some(c) = author_condition(&item, generation, now) {
                        author.push(c);
                    }
                }
                Ok(cel::Rendered::Omit) | Err(_) => {
                    complete = false;
                }
            }
        }
        let stamped = set.stamped(&prev, now);
        if complete && !author.is_empty() {
            let mut merged = author;
            merged.extend(stamped);
            status_body["conditions"] = json!(conditions::to_wire(&merged));
        } else {
            // incomplete evaluation: previously-persisted author
            // conditions ride the merge-with-previous law
            let merged = conditions::merge_with_previous(stamped, &prev);
            status_body["conditions"] = json!(conditions::to_wire(&merged));
        }
    }

    // The compare-then-patch guard: an identical status never re-patches
    // (a status write bumps the rv, the event returns, the loop spins).
    if !status_matches(instance.get("status").unwrap_or(&Json::Null), &status_body) {
        plan.actions.push(crate::face::Action::PatchStatus {
            target,
            namespace: (!meta_ns.is_empty()).then(|| meta_ns.clone()),
            name: meta_name.clone(),
            body: json!({"status": status_body}),
        });
    }

    plan.state = Some(state);
    // Applied-but-unconfirmed also requeues: the confirm event usually
    // drives the next pass, and the beat covers the missed event.
    plan.requeue = any_soft || hard_error.is_some() || any_unconfirmed;
    plan
}

/// The deletion plan: drain the highest wave, finalizer last.
#[allow(clippy::too_many_lines)] // the wave loop and the finalizer drop read best in one law
fn deletion_plan(
    graph: &CompiledGraph,
    instance: &Json,
    children: &Children,
    target: &crate::face::Target,
    meta_ns: &str,
    meta_name: &str,
    now: &str,
) -> Plan {
    let mut plan = Plan::default();
    let generation = instance
        .pointer("/metadata/generation")
        .and_then(Json::as_u64)
        .unwrap_or(1);
    let node_wave = |id: &str| {
        graph
            .deletion_waves
            .iter()
            .enumerate()
            .find_map(|(i, w)| {
                if w.iter().any(|x| x == id) {
                    Some(i)
                } else {
                    None
                }
            })
            .unwrap_or(0)
    };
    let mut remaining: Vec<(String, Option<String>, String, usize)> = Vec::new();
    for (gvr, rows) in children {
        let wave_of_row = |row: &Json| -> usize {
            row.pointer(NODE_ID_POINTER)
                .and_then(Json::as_str)
                .map_or(0, node_wave)
        };
        for row in rows {
            if owned_by_instance(row, instance) {
                remaining.push((
                    gvr.clone(),
                    row.pointer("/metadata/namespace")
                        .and_then(Json::as_str)
                        .map(str::to_owned),
                    row.pointer("/metadata/name")
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    wave_of_row(row),
                ));
            }
        }
    }

    if remaining.is_empty() {
        let finalizers: Vec<Json> = instance
            .pointer("/metadata/finalizers")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|f| f.as_str() != Some(labels::FINALIZER))
            .collect();
        plan.actions.push(crate::face::Action::Patch {
            target: target.clone(),
            namespace: (!meta_ns.is_empty()).then(|| meta_ns.to_owned()),
            name: meta_name.to_owned(),
            body: json!({"metadata": {"finalizers": finalizers}}),
        });
        // The drain law: the finalizer drop alone does not purge — the
        // next DELETE completes it (an already-purged row 404s and the
        // worker logs it at debug).
        plan.actions.push(crate::face::Action::Delete {
            target: target.clone(),
            namespace: (!meta_ns.is_empty()).then(|| meta_ns.to_owned()),
            name: meta_name.to_owned(),
        });
        plan.state = Some(PlanState::Deleting);
        return plan;
    }

    // The FIRST wave drains this pass (layer 0 = the dependents — they
    // delete before their dependencies); deeper waves wait for these
    // deletes' events (and the beat) to re-run this plan.
    let next = remaining.iter().map(|(_, _, _, w)| *w).min().unwrap_or(0);
    for (gvr, ns, name, wave) in &remaining {
        if *wave != next {
            continue;
        }
        let (group, version, plural) = split_gvr_key(gvr);
        plan.actions.push(crate::face::Action::Delete {
            target: crate::face::Target::cr(&group, &version, &plural, None),
            namespace: ns.clone(),
            name: name.clone(),
        });
    }

    let wire_conditions = instance
        .pointer("/status/conditions")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let prev = conditions::from_wire(&wire_conditions);
    let mut set = conditions::ConditionSet::for_object(&prev, generation);
    set.set(
        conditions::RESOURCES_READY,
        CondStatus::False,
        "UnderDeletion",
        &format!(
            "deleting wave {next} of {} owned resource(s)",
            remaining.len()
        ),
    );
    let status = json!({
        "state": PlanState::Deleting.wire(),
        "conditions": conditions::to_wire(&set.stamped(&prev, now)),
    });
    // The patch merges onto the stored status (the author's projected
    // fields ride along), so the skip-identical guard compares the
    // MERGED result — a raw compare against a state+conditions-only
    // intent never matches and the patch feeds itself (each write
    // echoes a Modified event back into a deleting pass).
    let wire_status = instance.get("status").unwrap_or(&Json::Null);
    if !status_matches(wire_status, &merge(wire_status, &status)) {
        plan.actions.push(crate::face::Action::PatchStatus {
            target: target.clone(),
            namespace: (!meta_ns.is_empty()).then(|| meta_ns.to_owned()),
            name: meta_name.to_owned(),
            body: json!({"status": status}),
        });
    }
    plan.state = Some(PlanState::Deleting);
    plan.requeue = true;
    plan
}

/// The finalizer/labels stamp: a patch only when something is missing.
fn ensure_managed(
    graph: &CompiledGraph,
    instance: &Json,
    target: &crate::face::Target,
    ns: &str,
    name: &str,
) -> Option<crate::face::Action> {
    let has_finalizer = instance
        .pointer("/metadata/finalizers")
        .and_then(Json::as_array)
        .is_some_and(|f| f.iter().any(|x| x.as_str() == Some(labels::FINALIZER)));
    let labels_ok = instance
        .pointer("/metadata/labels")
        .and_then(Json::as_object)
        .is_some_and(|l| {
            l.get(labels::RGD_NAME_LABEL).and_then(Json::as_str) == Some(graph.rgd_name.as_str())
        });
    if has_finalizer && labels_ok {
        return None;
    }
    let mut patch = json!({});
    if !has_finalizer {
        // merge-patch replaces arrays: carry the existing finalizers + ours
        let mut finalizers: Vec<Json> = instance
            .pointer("/metadata/finalizers")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        finalizers.push(json!(labels::FINALIZER));
        patch["metadata"]["finalizers"] = json!(finalizers);
    }
    if !labels_ok {
        patch["metadata"]["labels"] = json!({labels::RGD_NAME_LABEL: graph.rgd_name});
    }
    Some(crate::face::Action::Patch {
        target: target.clone(),
        namespace: (!ns.is_empty()).then(|| ns.to_owned()),
        name: name.to_owned(),
        body: patch,
    })
}

struct Item {
    bindings: BTreeMap<String, Json>,
}

/// Expand the cartesian product of the node's forEach bindings. A node
/// without forEach yields one binding-less item.
fn expand_for_each(node: &crate::graph::CompiledNode, scope: &Scope) -> Result<Vec<Item>, String> {
    if node.for_each.is_empty() {
        return Ok(vec![Item {
            bindings: BTreeMap::new(),
        }]);
    }
    let mut product: Vec<Item> = vec![Item {
        bindings: BTreeMap::new(),
    }];
    for (name, expr) in &node.for_each {
        let v = cel::eval_value(expr, scope)?;
        let Json::Array(items) = v else {
            return Err(format!("iterator {name:?} must evaluate to a list"));
        };
        let mut next = Vec::with_capacity(product.len() * items.len());
        for item in &product {
            for element in &items {
                let mut bindings = item.bindings.clone();
                bindings.insert(name.clone(), element.clone());
                next.push(Item { bindings });
            }
        }
        product = next;
    }
    Ok(product)
}

/// The per-item scopes a collection's readyWhen evaluates against.
fn item_scopes(items: &[Item], base: &Scope) -> Vec<Scope> {
    items
        .iter()
        .map(|item| {
            let mut s = base.clone();
            for (k, v) in &item.bindings {
                s.insert(k.clone(), v.clone());
            }
            if let Some(first) = item.bindings.values().next() {
                s.insert("each".to_owned(), first.clone());
            }
            s
        })
        .collect()
}

/// Render the template against a scope: every expression evaluates, the
/// omit sentinel drops its field, whole-expressions take the CEL value.
fn render_value(v: &Json, scope: &Scope) -> Result<Option<Json>, String> {
    match v {
        Json::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, child) in map {
                if let Some(rendered) = render_value(child, scope)? {
                    out.insert(k.clone(), rendered);
                }
            }
            Ok(Some(Json::Object(out)))
        }
        Json::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                if let Some(rendered) = render_value(item, scope)? {
                    out.push(rendered);
                }
            }
            Ok(Some(Json::Array(out)))
        }
        Json::String(s) => {
            let render = crate::expression::classify(s)?;
            match cel::eval_render(&render, &mut |expr| cel::eval_program(expr, scope))? {
                cel::Rendered::Omit => Ok(None),
                cel::Rendered::Value(j) => Ok(Some(j)),
            }
        }
        other => Ok(Some(other.clone())),
    }
}

/// Evaluate a node's includeWhen (all must hold).
fn eval_conditions(exprs: &[String], scope: &Scope) -> Result<bool, String> {
    for e in exprs {
        match cel::eval(e, scope)? {
            cel::Rendered::Value(Json::Bool(b)) => {
                if !b {
                    return Ok(false);
                }
            }
            _ => return Err(format!("{e:?} must evaluate to a bool")),
        }
    }
    Ok(true)
}

/// Evaluate a node's readiness across its items (every item satisfies
/// every expression).
#[allow(clippy::needless_pass_by_value)] // the scopes ride readyWhen's evaluation only
fn eval_ready(node: &crate::graph::CompiledNode, scopes: Vec<Scope>) -> Result<bool, String> {
    if node.ready_when.is_empty() {
        return Ok(true);
    }
    if scopes.is_empty() {
        return Ok(false);
    }
    for s in &scopes {
        for e in &node.ready_when {
            match cel::eval(e, s)? {
                cel::Rendered::Value(Json::Bool(b)) => {
                    if !b {
                        return Ok(false);
                    }
                }
                _ => return Err(format!("{e:?} must evaluate to a bool")),
            }
        }
    }
    Ok(true)
}

/// Stamp the engine's identity marks onto a rendered child.
fn stamp_child(
    node: &crate::graph::CompiledNode,
    mut rendered: Json,
    instance_ns: &str,
    instance: &Json,
    graph: &CompiledGraph,
) -> Json {
    let Some(obj) = rendered.as_object_mut() else {
        return rendered;
    };
    let Some(meta) = obj
        .entry("metadata")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        return rendered;
    };
    let mut stamp = serde_json::Map::new();
    stamp.insert(labels::NODE_ID_LABEL.to_owned(), json!(node.id));
    stamp.insert(labels::OWNED_LABEL.to_owned(), json!("true"));
    stamp.insert(
        labels::MANAGED_BY_KEY.to_owned(),
        json!(labels::MANAGED_BY_VALUE),
    );
    stamp.insert(
        labels::INSTANCE_NAME_LABEL.to_owned(),
        json!(instance_name(instance)),
    );
    // The kind label routes the child's events back to the owning
    // instance (the engine's child-event handler matches RGDs by it).
    if let Some(kind) = instance.get("kind").and_then(Json::as_str) {
        stamp.insert(labels::INSTANCE_KIND_LABEL.to_owned(), json!(kind));
    }
    // The instance's uid is the ownership join — a recreated instance
    // with the same name must not inherit the old children.
    stamp.insert(
        labels::INSTANCE_UID_LABEL.to_owned(),
        json!(
            instance
                .pointer("/metadata/uid")
                .and_then(Json::as_str)
                .unwrap_or_default()
        ),
    );
    stamp.insert(labels::RGD_NAME_LABEL.to_owned(), json!(graph.rgd_name));
    let inst_ns = instance
        .pointer("/metadata/namespace")
        .and_then(Json::as_str)
        .unwrap_or(instance_ns);
    if !inst_ns.is_empty() {
        stamp.insert(labels::INSTANCE_NAMESPACE_LABEL.to_owned(), json!(inst_ns));
    }
    stamp.insert(
        labels::INSTANCE_KIND_LABEL.to_owned(),
        json!(graph.instance_kind),
    );
    stamp.insert(
        labels::INSTANCE_GROUP_LABEL.to_owned(),
        json!(graph.instance_group),
    );
    stamp.insert(
        labels::INSTANCE_VERSION_LABEL.to_owned(),
        json!(graph.instance_version),
    );
    if let Some(l) = meta
        .entry("labels")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    {
        l.extend(stamp);
    }
    // The default namespace: a namespaced child lands in the instance's
    // namespace unless the template named one; an empty namespace key
    // comes off (cluster-scoped rows carry none).
    let ns = meta
        .get("namespace")
        .and_then(Json::as_str)
        .unwrap_or_default();
    if ns.is_empty() {
        if inst_ns.is_empty() {
            meta.remove("namespace");
        } else {
            meta.insert("namespace".to_owned(), json!(inst_ns));
        }
    }
    rendered
}

/// The merge-patch body for one child: only when an owned field differs.
fn child_patch(current: &Json, desired: &Json) -> Option<Json> {
    let strip_unpatchable = |v: &Json| -> Json {
        let mut v = v.clone();
        if let Some(o) = v.as_object_mut() {
            o.remove("status");
            // identity fields a patch can never change
            for k in ["apiVersion", "kind"] {
                o.remove(k);
            }
            // the server's bookkeeping never rides the patch
            if let Some(m) = o.get_mut("metadata").and_then(Json::as_object_mut) {
                for k in [
                    "resourceVersion",
                    "uid",
                    "creationTimestamp",
                    "generation",
                    "managedFields",
                ] {
                    m.remove(k);
                }
            }
        }
        v
    };
    let current = strip_unpatchable(current);
    let desired = strip_unpatchable(desired);
    // ONLY OWNED FIELDS MATTER. Other controllers (the Service
    // controller's clusterIP, a Deployment default, an admission
    // mutation) add fields the template never named. A full equality
    // would see a difference on every pass and re-patch forever — a
    // node can never confirm, so the instance stays IN_PROGRESS. Treat
    // the desired object as an intent SUBSET of the stored one: extra
    // server fields are fine, only a missing or differing desired field
    // is a real drift worth patching.
    if json_subset(&desired, &current) {
        return None;
    }
    Some(desired)
}

/// Is `intent` fully contained in `stored`? Objects compare key by key
/// (extra stored keys are fine); arrays and scalars compare exactly.
fn json_subset(intent: &Json, stored: &Json) -> bool {
    match (intent, stored) {
        (Json::Object(i), Json::Object(s)) => i
            .iter()
            .all(|(k, v)| s.get(k).is_some_and(|sv| json_subset(v, sv))),
        _ => intent == stored,
    }
}
fn child_target(node: &crate::graph::CompiledNode, ns: Option<&str>) -> crate::face::Target {
    crate::face::Target::cr(
        node.group.as_deref().unwrap_or(""),
        &node.version,
        &node.plural,
        ns,
    )
}

/// The children-cache key: exactly the pump target's key, so the engine
/// and the reconciler never disagree about where a child lives.
#[must_use]
pub fn gvr_key(node: &crate::graph::CompiledNode) -> String {
    child_target(node, None).key()
}

fn split_gvr_key(key: &str) -> (String, String, String) {
    let mut parts = key.split('/');
    // `Target::key()` spells the empty (core-v1) group as `"core"`;
    // normalize it back so `Target::cr("", ..)` maps it to `None` and
    // the DELETE hits `/api/v1/...`, not `/apis/core/v1/...`.
    let g = match parts.next().unwrap_or_default() {
        "core" => String::new(),
        other => other.to_owned(),
    };
    let v = parts.next().unwrap_or_default().to_owned();
    // the key's last segment is `plural@namespace` — the namespace is
    // cache-key bookkeeping, not part of the plural
    let p = parts.next().unwrap_or_default();
    let plural = p.split('@').next().unwrap_or_default().to_owned();
    (g, v, plural)
}

pub fn instance_name(instance: &Json) -> String {
    instance
        .pointer("/metadata/name")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Does this row carry the engine's instance labels for this instance?
pub fn owned_by_instance(row: &Json, instance: &Json) -> bool {
    if row
        .pointer("/metadata/labels/kro.run~1owned")
        .and_then(Json::as_str)
        != Some("true")
    {
        return false;
    }
    let uid = row.pointer(INSTANCE_UID_POINTER).and_then(Json::as_str);
    let inst_uid = instance.pointer("/metadata/uid").and_then(Json::as_str);
    if let (Some(u), Some(iu)) = (uid, inst_uid)
        && !iu.is_empty()
    {
        return u == iu;
    }
    let name_label = row.pointer(INSTANCE_NAME_POINTER).and_then(Json::as_str);
    let name = instance_name(instance);
    let ns_label = row.pointer(INSTANCE_NS_POINTER).and_then(Json::as_str);
    let ns = instance
        .pointer("/metadata/namespace")
        .and_then(Json::as_str)
        .unwrap_or_default();
    name_label == Some(name.as_str()) && (ns_label == Some(ns) || ns.is_empty())
}

fn same_object(row: &Json, ns: Option<&str>, name: &str) -> bool {
    let row_name = row.pointer("/metadata/name").and_then(Json::as_str);
    let row_ns = row.pointer("/metadata/namespace").and_then(Json::as_str);
    row_name == Some(name)
        && match (row_ns, ns) {
            (Some(r), Some(n)) => r == n,
            (None, _) => true,
            (Some(_), None) => false,
        }
}

/// Canonical-JSON compare of the status body vs the wire (the
/// skip-identical guard).
fn status_matches(wire: &Json, computed: &Json) -> bool {
    serde_json::to_vec(wire).unwrap_or_default() == serde_json::to_vec(computed).unwrap_or_default()
}

fn merge(current: &Json, patch: &Json) -> Json {
    let mut out = current.clone();
    json_patch_merge(&mut out, patch);
    out
}

/// RFC 7386 merge (the flavor the surfaces serve).
fn json_patch_merge(dst: &mut Json, src: &Json) {
    match (dst, src) {
        (Json::Object(d), Json::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        json_patch_merge(existing, v);
                    }
                    _ => {
                        d.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (d, s) => *d = s.clone(),
    }
}

fn author_condition(item: &Json, generation: u64, now: &str) -> Option<Condition> {
    let kind = item.get("type").and_then(Json::as_str)?;
    let status = match item.get("status").and_then(Json::as_str) {
        Some("True") => CondStatus::True,
        Some("False") => CondStatus::False,
        _ => CondStatus::Unknown,
    };
    Some(Condition {
        kind: kind.to_owned(),
        status,
        reason: item.get("reason").and_then(Json::as_str).map(str::to_owned),
        message: item
            .get("message")
            .and_then(Json::as_str)
            .map(str::to_owned),
        observed_generation: generation,
        last_transition_time: Some(now.to_owned()),
    })
}

/// Insert a rendered value at a status field path (nested maps only).
fn insert_at(fields: &mut serde_json::Map<String, Json>, path: &[Seg], v: Json) {
    if path.is_empty() {
        return;
    }
    if let Seg::Key(k) = &path[0] {
        if path.len() == 1 {
            fields.insert(k.clone(), v);
            return;
        }
        let entry = fields.entry(k.clone()).or_insert_with(|| json!({}));
        if let Some(map) = entry.as_object_mut() {
            insert_at(map, &path[1..], v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;
    use crate::graph::{EngineDefaults, compile};
    use serde_json::json;

    fn graph_of(resources: &Json, status: &Json) -> CompiledGraph {
        let body = json!({
            "metadata": {"name": "webstore", "uid": "rgd-uid", "generation": 1},
            "spec": {
                "schema": {
                    "apiVersion": "v1alpha1",
                    "kind": "WebStore",
                    "spec": {"name": "string | required=true", "image": "string | default=nginx",
                             "monitoring": "boolean | default=false"},
                    "status": status
                },
                "resources": resources
            }
        });
        let parsed = api::parse(&body, "kro.run").unwrap();
        compile(&parsed, &EngineDefaults::default()).unwrap()
    }

    fn instance(spec: &Json) -> Json {
        json!({
            "apiVersion": "kro.run/v1alpha1", "kind": "WebStore",
            "metadata": {"name": "shop", "namespace": "acme", "uid": "inst-1", "generation": 1},
            "spec": spec
        })
    }

    fn verbs(plan: &Plan) -> Vec<&'static str> {
        plan.actions.iter().map(crate::face::Action::verb).collect()
    }

    /// The exact label family the engine stamps onto a child — the
    /// fixtures carry it so a converged child never re-patches.
    fn stamped_labels(node: &str) -> serde_json::Value {
        serde_json::json!({
            "kro.run/node-id": node,
            "kro.run/owned": "true",
            "app.kubernetes.io/managed-by": "kro-rs",
            "kro.run/instance-name": "shop",
            "kro.run/instance-uid": "inst-1",
            "kro.run/instance-namespace": "acme",
            "kro.run/instance-kind": "WebStore",
            "kro.run/instance-group": "kro.run",
            "kro.run/instance-version": "v1alpha1",
            "kro.run/rgd-name": "webstore"
        })
    }

    #[test]
    fn a_fresh_instance_creates_children_and_statuses_in_progress() {
        let g = graph_of(
            &json!([
                {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {"name": "${schema.spec.name}--cm"}, "data": {"image": "${schema.spec.image}"}}},
                {"id": "deployment", "template": {"apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": {"name": "${schema.spec.name}"},
                    "spec": {"template": {"spec": {"containers": [{"image": "${configmap.data.image}"}]}}}}}
            ]),
            &json!({"imageOut": "${configmap.data.image}"}),
        );
        let mut inst = instance(&json!({"name": "shop", "image": "nginx:1"}));
        // Pass 1: the configmap submits; the deployment GATES on it
        // (GateReadiness — a dependent never creates before its
        // dependency is observed ready).
        let plan = reconcile(&g, &inst, &Children::new(), "2026-01-01T00:00:00Z");
        assert_eq!(verbs(&plan), vec!["patch", "create", "patch-status"]);
        // the finalizer patch comes first, the dependency child next
        match &plan.actions[0] {
            crate::face::Action::Patch { body, .. } => {
                assert_eq!(
                    body["metadata"]["finalizers"][0],
                    json!(crate::labels::FINALIZER)
                );
            }
            other => panic!("expected the manage patch, got {other:?}"),
        }
        match &plan.actions[1] {
            crate::face::Action::Create { body, target, .. } => {
                assert_eq!(body["kind"], json!("ConfigMap"));
                assert_eq!(body["metadata"]["name"], json!("shop--cm"));
                assert_eq!(body["metadata"]["namespace"], json!("acme"));
                assert_eq!(target.plural, "configmaps");
                assert_eq!(
                    body["metadata"]["labels"]["kro.run/node-id"],
                    json!("configmap")
                );
                assert_eq!(body["metadata"]["labels"]["kro.run/owned"], json!("true"));
            }
            other => panic!("expected the configmap create, got {other:?}"),
        }
        // the manage patch converged (finalizer + rgd label landed)
        if let crate::face::Action::Patch { body, .. } = &plan.actions[0] {
            json_patch_merge(&mut inst, body);
        }
        // Pass 2: the configmap's create event returned (the cache holds
        // the confirmed, fully-stamped row) — the deployment applies and
        // reads the configmap's data through the scope.
        let confirmed = Children::from([(
            "core/v1/configmaps@*".to_owned(),
            vec![json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {
                    "name": "shop--cm", "namespace": "acme",
                    "labels": stamped_labels("configmap"),
                },
                "data": {"image": "nginx:1"}
            })],
        )]);
        let plan = reconcile(&g, &inst, &confirmed, "2026-01-01T00:00:00Z");
        assert_eq!(
            verbs(&plan),
            vec!["create", "patch-status"],
            "the gated dependent applies once its dependency is observed: {:?}",
            plan.actions
        );
        match &plan.actions[0] {
            crate::face::Action::Create { body, .. } => {
                assert_eq!(body["kind"], json!("Deployment"));
                assert_eq!(
                    body["spec"]["template"]["spec"]["containers"][0]["image"],
                    json!("nginx:1")
                );
            }
            other => panic!("expected the deployment create, got {other:?}"),
        }
        let st = match &plan.actions[1] {
            crate::face::Action::PatchStatus { body, .. } => body["status"].clone(),
            other => panic!("expected a status patch, got {other:?}: {:?}", plan.actions),
        };
        assert_eq!(
            st["state"],
            json!("IN_PROGRESS"),
            "pass 2: the deployment is submitted, not confirmed: {st}"
        );
        assert_eq!(plan.state, Some(PlanState::InProgress));
        assert!(plan.requeue);
    }

    #[test]
    fn ready_children_flip_the_instance_active_and_project_status() {
        let g = graph_of(
            &json!([
                {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {"name": "${schema.spec.name}--cm"}, "data": {"host": "db.internal"}},
                 "readyWhen": ["${configmap.data.host == \"db.internal\"}"]}
            ]),
            &json!({"endpoint": "${configmap.data.host}"}),
        );
        let children = Children::from([(
            "core/v1/configmaps@*".to_owned(),
            vec![json!({
                "metadata": {"name": "shop--cm", "namespace": "acme",
                    "labels": stamped_labels("configmap")},
                "data": {"host": "db.internal"}
            })],
        )]);
        // the instance already carries the finalizer + labels
        let mut inst = instance(&json!({"name": "shop"}));
        inst["metadata"]["finalizers"] = json!([crate::labels::FINALIZER]);
        inst["metadata"]["labels"] = json!({"kro.run/rgd-name": "webstore"});
        let plan = reconcile(&g, &inst, &children, "2026-01-01T00:00:00Z");
        assert_eq!(
            verbs(&plan),
            vec!["patch-status"],
            "no child writes on a converged graph: {:?}",
            plan.actions
        );
        let st = match &plan.actions[0] {
            crate::face::Action::PatchStatus { body, .. } => body["status"].clone(),
            other => panic!("expected a status patch, got {other:?}"),
        };
        eprintln!("READY-STATUS: {st}");
        assert_eq!(st["state"], json!("ACTIVE"));
        assert_eq!(
            st["endpoint"],
            json!("db.internal"),
            "the author status field projects"
        );
        let conds = st["conditions"].as_array().expect("conditions");
        assert!(
            conds
                .iter()
                .any(|c| c["type"] == json!("Ready") && c["status"] == json!("True"))
        );
        assert_eq!(plan.state, Some(PlanState::Active));
        assert!(!plan.requeue);
    }

    #[test]
    fn include_when_false_skips_and_prunes_the_orphan() {
        let g = graph_of(
            &json!([
                {"id": "monitor", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {"name": "${schema.spec.name}--monitor"}},
                 "includeWhen": ["${schema.spec.monitoring == true}"]}
            ]),
            &json!({}),
        );
        let mut inst = instance(&json!({"name": "shop", "monitoring": false}));
        inst["metadata"]["finalizers"] = json!([crate::labels::FINALIZER]);
        inst["metadata"]["labels"] = json!({"kro.run/rgd-name": "webstore"});
        let children = Children::from([(
            "core/v1/configmaps@*".to_owned(),
            vec![json!({
                "metadata": {"name": "shop--monitor", "namespace": "acme",
                    "labels": stamped_labels("monitor")},
                "data": {}
            })],
        )]);
        let plan = reconcile(&g, &inst, &children, "2026-01-01T00:00:00Z");
        assert!(
            plan.actions.iter().any(
                |a| matches!(a, crate::face::Action::Delete { name, .. } if name == "shop--monitor")
            ),
            "the skipped node's child prunes: {:?}",
            plan.actions
        );
        assert_eq!(plan.state, Some(PlanState::Active));
    }

    #[test]
    fn deletion_drains_waves_then_drops_the_finalizer() {
        let g = graph_of(
            &json!([
                {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {"name": "${schema.spec.name}--cm"}, "data": {"image": "nginx"}}},
                {"id": "deployment", "template": {"apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": {"name": "${schema.spec.name}"},
                    "spec": {"template": {"spec": {"containers": [{"image": "${configmap.data.image}"}]}}}}}
            ]),
            &json!({}),
        );
        // deployment is the dependent wave; configmap the dependency wave
        let mut inst = instance(&json!({"name": "shop"}));
        inst["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        inst["metadata"]["finalizers"] = json!([crate::labels::FINALIZER]);
        let owned = |node: &str, name: &str| {
            json!({
                "metadata": {"name": name, "namespace": "acme",
                    "labels": {"kro.run/node-id": node, "kro.run/owned": "true",
                               "kro.run/instance-uid": "inst-1", "kro.run/instance-name": "shop"}}
            })
        };
        let children = Children::from([
            (
                "core/v1/configmaps@*".to_owned(),
                vec![owned("configmap", "shop--cm")],
            ),
            (
                "apps/v1/deployments@*".to_owned(),
                vec![owned("deployment", "shop")],
            ),
        ]);
        // wave 1: the dependents (deployment) delete first
        let plan = reconcile(&g, &inst, &children, "2026-01-01T00:00:00Z");
        let deletes: Vec<&String> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                crate::face::Action::Delete { name, .. } => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(deletes, vec!["shop"], "the dependent wave drains first");
        assert_eq!(plan.state, Some(PlanState::Deleting));
        // wave 2: once the deployment is gone, the configmap goes
        let children = Children::from([(
            "core/v1/configmaps@*".to_owned(),
            vec![owned("configmap", "shop--cm")],
        )]);
        let plan = reconcile(&g, &inst, &children, "2026-01-01T00:00:00Z");
        let deletes: Vec<&String> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                crate::face::Action::Delete { name, .. } => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(deletes, vec!["shop--cm"]);
        // wave 3: the inventory drained — the finalizer drops
        let plan = reconcile(&g, &inst, &Children::new(), "2026-01-01T00:00:00Z");
        match &plan.actions[0] {
            crate::face::Action::Patch { body, .. } => {
                assert_eq!(body["metadata"]["finalizers"], json!([]));
            }
            other => panic!("expected the finalizer patch, got {other:?}"),
        }
    }

    #[test]
    fn for_each_expands_the_cartesian_product() {
        let g = graph_of(
            &json!([
                {"id": "svc", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {"name": "${schema.spec.name}-${region}"}},
                 "forEach": [{"region": "${schema.spec.regions}"}]}
            ]),
            &json!({}),
        );
        let mut inst = instance(&json!({"name": "shop", "regions": ["eu", "us"]}));
        inst["metadata"]["finalizers"] = json!([crate::labels::FINALIZER]);
        inst["metadata"]["labels"] = json!({"kro.run/rgd-name": "webstore"});
        let plan = reconcile(&g, &inst, &Children::new(), "2026-01-01T00:00:00Z");
        let names: Vec<String> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                crate::face::Action::Create { body, .. } => Some(
                    body["metadata"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["shop-eu", "shop-us"]);
    }

    #[test]
    fn an_unsatisfied_ready_when_gates_the_dependents() {
        let g = graph_of(
            &json!([
                {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {"name": "${schema.spec.name}--cm"}, "data": {"phase": "Pending"}},
                 "readyWhen": ["${configmap.data.phase == \"Running\"}"]},
                {"id": "deployment", "template": {"apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": {"name": "${schema.spec.name}"},
                    "spec": {"replicas": "${configmap.data.replicas}"}}}
            ]),
            &json!({}),
        );
        let mut inst = instance(&json!({"name": "shop"}));
        inst["metadata"]["finalizers"] = json!([crate::labels::FINALIZER]);
        inst["metadata"]["labels"] = json!({"kro.run/rgd-name": "webstore"});
        let children = Children::from([(
            "core/v1/configmaps@*".to_owned(),
            vec![json!({
                "metadata": {"name": "shop--cm", "namespace": "acme",
                    "labels": stamped_labels("configmap")},
                "data": {"phase": "Pending"}
            })],
        )]);
        let plan = reconcile(&g, &inst, &children, "2026-01-01T00:00:00Z");
        let creates = plan
            .actions
            .iter()
            .filter(|a| matches!(a, crate::face::Action::Create { .. }))
            .count();
        assert_eq!(creates, 0, "the gated dependent never creates");
        assert_eq!(plan.state, Some(PlanState::InProgress));
        assert!(plan.requeue);
    }

    #[test]
    fn a_status_write_does_not_repatch_when_identical() {
        let g = graph_of(
            &json!([
                {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {"name": "${schema.spec.name}--cm"}, "data": {"host": "db.internal"}},
                 "readyWhen": ["${configmap.data.host == \"db.internal\"}"]}
            ]),
            &json!({"endpoint": "${configmap.data.host}"}),
        );
        let mut inst = instance(&json!({"name": "shop"}));
        inst["metadata"]["finalizers"] = json!([crate::labels::FINALIZER]);
        inst["metadata"]["labels"] = json!({"kro.run/rgd-name": "webstore"});
        let children = Children::from([(
            "core/v1/configmaps@*".to_owned(),
            vec![json!({
                "metadata": {"name": "shop--cm", "namespace": "acme",
                    "labels": stamped_labels("configmap")},
                "data": {"host": "db.internal"}
            })],
        )]);
        let first = reconcile(&g, &inst, &children, "2026-01-01T00:00:00Z");
        // apply the status patch to the instance and re-reconcile
        let st = first
            .actions
            .iter()
            .find_map(|a| match a {
                crate::face::Action::PatchStatus { body, .. } => Some(body["status"].clone()),
                _ => None,
            })
            .expect("a status patch");
        inst["status"] = st;
        let second = reconcile(&g, &inst, &children, "2026-01-01T00:00:00Z");
        assert!(
            second.actions.is_empty(),
            "the patch-loop guard holds: {:?}",
            second.actions
        );
    }

    #[test]
    fn a_controller_added_field_does_not_cause_a_patch_loop() {
        // The Service controller stamps spec.clusterIP on a Service the
        // RGD template never named. The reconciler must treat the
        // template as an intent SUBSET of the stored child, not a full
        // equality — otherwise every pass re-patches and the node never
        // confirms (the instance stays IN_PROGRESS forever).
        let desired = json!({"spec": {"selector": {"app": "hello"}, "type": "ClusterIP"}});
        let stored = json!({"spec": {
            "selector": {"app": "hello"},
            "type": "ClusterIP",
            "clusterIP": "10.86.192.70"
        }});
        assert_eq!(super::child_patch(&stored, &desired), None);
        // A real drift in an owned field still patches.
        let drifted = json!({"spec": {"selector": {"app": "other"}, "clusterIP": "10.86.192.70"}});
        assert!(super::child_patch(&drifted, &desired).is_some());
    }
}
