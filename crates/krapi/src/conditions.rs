//! THE CONDITION SET LAW — knative-style conditions with a root (`Ready`)
//! and its dependents. A condition set initializes every missing condition
//! to `Unknown`, folds dependent verdicts into the root, stamps
//! `observedGeneration`, and preserves `lastTransitionTime` when the
//! status did not change. The merge-with-previous rule keeps previously
//! persisted conditions alive while their expression cannot produce
//! output (a resource gone not-ready must not silently vanish from the
//! wire).

use serde_json::{Value as Json, json};

/// The built-in instance condition types (the root and its dependents).
pub const READY: &str = "Ready";
pub const INSTANCE_MANAGED: &str = "InstanceManaged";
pub const GRAPH_RESOLVED: &str = "GraphResolved";
pub const RESOURCES_READY: &str = "ResourcesReady";

/// The condition types the controller owns (author conditions join these
/// on the wire).
#[must_use]
pub fn builtin_types() -> [&'static str; 4] {
    [READY, INSTANCE_MANAGED, GRAPH_RESOLVED, RESOURCES_READY]
}

/// One condition in wire shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub kind: String,
    pub status: Status,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub observed_generation: u64,
    pub last_transition_time: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    True,
    False,
    Unknown,
}

impl Status {
    /// The k8s wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::True => "True",
            Status::False => "False",
            Status::Unknown => "Unknown",
        }
    }
}

/// A condition set over one object's conditions.
pub struct ConditionSet {
    conditions: Vec<Condition>,
    generation: u64,
}

impl ConditionSet {
    /// Build the set from the object's current conditions, initializing
    /// every built-in type that is absent to `Unknown` (the root first,
    /// for stable transition times).
    #[must_use]
    pub fn for_object(conditions: &[Condition], generation: u64) -> Self {
        let mut list = conditions.to_vec();
        let ensure = |kind: &str, list: &mut Vec<Condition>| {
            if !list.iter().any(|c| c.kind == kind) {
                list.push(Condition {
                    kind: kind.to_owned(),
                    status: Status::Unknown,
                    reason: None,
                    message: None,
                    observed_generation: generation,
                    last_transition_time: None,
                });
            }
        };
        ensure(READY, &mut list);
        ensure(INSTANCE_MANAGED, &mut list);
        ensure(GRAPH_RESOLVED, &mut list);
        ensure(RESOURCES_READY, &mut list);
        Self {
            conditions: list,
            generation,
        }
    }

    /// Set one condition True/False with reason and message.
    pub fn set(&mut self, kind: &str, status: Status, reason: &str, message: &str) {
        for c in &mut self.conditions {
            if c.kind == kind {
                c.status = status;
                c.reason = (!reason.is_empty()).then(|| reason.to_owned());
                c.message = (!message.is_empty()).then(|| message.to_owned());
                c.observed_generation = self.generation;
                return;
            }
        }
        self.conditions.push(Condition {
            kind: kind.to_owned(),
            status,
            reason: (!reason.is_empty()).then(|| reason.to_owned()),
            message: (!message.is_empty()).then(|| message.to_owned()),
            observed_generation: self.generation,
            last_transition_time: None,
        });
    }

    /// The root verdict: `Ready` is True iff it is True on the wire.
    #[must_use]
    pub fn root_ready(&self) -> bool {
        self.conditions
            .iter()
            .any(|c| c.kind == READY && c.status == Status::True)
    }

    /// THE FOLD: the root condition is the aggregation of its dependents —
    /// True only when every dependent is True, False when any is False,
    /// Unknown otherwise. A root that never folds lies on the wire.
    pub fn fold_root(&mut self, dependents: &[&str]) {
        let mut verdict = Status::True;
        for c in &self.conditions {
            if !dependents.contains(&c.kind.as_str()) {
                continue;
            }
            match (c.status, verdict) {
                (Status::False, _) => verdict = Status::False,
                (Status::Unknown, Status::True) => verdict = Status::Unknown,
                _ => {}
            }
        }
        self.set(READY, verdict, "Ready", "the dependents' aggregation");
    }

    /// The stamped list: `lastTransitionTime` survives when the status
    /// did not change; every built-in carries its stamp time.
    #[must_use]
    pub fn stamped(&self, previous: &[Condition], now: &str) -> Vec<Condition> {
        let mut out = Vec::with_capacity(self.conditions.len());
        for c in &self.conditions {
            let mut stamped = c.clone();
            if let Some(prev) = previous.iter().find(|p| p.kind == c.kind)
                && prev.status == c.status
                && prev.last_transition_time.is_some()
            {
                stamped
                    .last_transition_time
                    .clone_from(&prev.last_transition_time);
            }
            if stamped.last_transition_time.is_none() {
                stamped.last_transition_time = Some(now.to_owned());
            }
            out.push(stamped);
        }
        out
    }

    /// The raw list (pre-stamp).
    #[must_use]
    pub fn list(&self) -> &[Condition] {
        &self.conditions
    }
}

/// Merge the freshly-evaluated conditions with the previous wire list:
/// previously-persisted types the current evaluation could not produce
/// stay on the wire.
#[must_use]
pub fn merge_with_previous(current: Vec<Condition>, previous: &[Condition]) -> Vec<Condition> {
    let have: std::collections::BTreeSet<String> = current.iter().map(|c| c.kind.clone()).collect();
    let mut out = current;
    for p in previous {
        if !have.contains(&p.kind) {
            out.push(p.clone());
        }
    }
    out
}

/// Conditions → wire shape (the instance status patch body's list).
#[must_use]
pub fn to_wire(conditions: &[Condition]) -> Vec<Json> {
    conditions
        .iter()
        .map(|c| {
            let mut o = json!({
                "type": c.kind,
                "status": c.status.as_str(),
                "observedGeneration": c.observed_generation,
            });
            if let Some(r) = &c.reason {
                o["reason"] = json!(r);
            }
            if let Some(m) = &c.message {
                o["message"] = json!(m);
            }
            if let Some(t) = &c.last_transition_time {
                o["lastTransitionTime"] = json!(t);
            }
            o
        })
        .collect()
}

/// Wire conditions → typed (malformed entries skipped).
#[must_use]
pub fn from_wire(list: &[Json]) -> Vec<Condition> {
    list.iter()
        .filter_map(|c| {
            let kind = c.get("type").and_then(Json::as_str)?.to_owned();
            let status = match c.get("status").and_then(Json::as_str) {
                Some("True") => Status::True,
                Some("False") => Status::False,
                _ => Status::Unknown,
            };
            Some(Condition {
                kind,
                status,
                reason: c.get("reason").and_then(Json::as_str).map(str::to_owned),
                message: c.get("message").and_then(Json::as_str).map(str::to_owned),
                observed_generation: c
                    .get("observedGeneration")
                    .and_then(Json::as_u64)
                    .unwrap_or(0),
                last_transition_time: c
                    .get("lastTransitionTime")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> String {
        "2026-01-01T00:00:00Z".to_owned()
    }

    #[test]
    fn missing_builtins_initialize_unknown() {
        let set = ConditionSet::for_object(&[], 1);
        let list = set.list();
        assert_eq!(list.len(), 4);
        assert!(list.iter().all(|c| c.status == Status::Unknown));
        assert_eq!(list[0].kind, READY);
    }

    #[test]
    fn transition_time_survives_same_status() {
        let prev = vec![Condition {
            kind: READY.to_owned(),
            status: Status::False,
            reason: None,
            message: None,
            observed_generation: 1,
            last_transition_time: Some("2025-01-01T00:00:00Z".to_owned()),
        }];
        let mut set = ConditionSet::for_object(&prev, 2);
        set.set(READY, Status::False, "NotReady", "still waiting");
        let stamped = set.stamped(&prev, &now());
        let ready = stamped.iter().find(|c| c.kind == READY).expect("ready");
        assert_eq!(
            ready.last_transition_time.as_deref(),
            Some("2025-01-01T00:00:00Z"),
            "same status, same stamp"
        );
        let managed = stamped
            .iter()
            .find(|c| c.kind == INSTANCE_MANAGED)
            .expect("managed");
        assert_eq!(
            managed.last_transition_time.as_deref(),
            Some(now().as_str())
        );
    }

    #[test]
    fn merge_with_previous_keeps_unproduced_types() {
        let prev = vec![
            Condition {
                kind: "Deployed".to_owned(),
                status: Status::True,
                reason: None,
                message: None,
                observed_generation: 1,
                last_transition_time: None,
            },
            Condition {
                kind: READY.to_owned(),
                status: Status::False,
                reason: None,
                message: None,
                observed_generation: 1,
                last_transition_time: None,
            },
        ];
        let current = vec![Condition {
            kind: READY.to_owned(),
            status: Status::True,
            reason: None,
            message: None,
            observed_generation: 2,
            last_transition_time: None,
        }];
        let merged = merge_with_previous(current, &prev);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|c| c.kind == "Deployed"));
    }

    #[test]
    fn wire_round_trip() {
        let c = vec![Condition {
            kind: READY.to_owned(),
            status: Status::True,
            reason: Some("AllReady".to_owned()),
            message: None,
            observed_generation: 3,
            last_transition_time: Some("2026-01-01T00:00:00Z".to_owned()),
        }];
        let wire = to_wire(&c);
        assert_eq!(wire[0]["status"], json!("True"));
        assert_eq!(wire[0]["reason"], json!("AllReady"));
        assert!(wire[0].get("message").is_none());
        let back = from_wire(&wire);
        assert_eq!(back, c);
    }
}
