#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
// THE INDEXING ALLOW (the kapi controller's own convention — the crate
// this one embeds beside): the SimpleSchema converter, the CEL
// expression walker, and the reconciler slice strings and vectors at
// invariant-guarded positions throughout; per-site allows would bury
// the laws under noise.
#![allow(clippy::indexing_slicing)]
//! # kro-rs — the kube resource orchestrator engine
//!
//! A [`ResourceGraphDefinition`](api::ResourceGraphDefinition) declares a new
//! custom API (its schema, in `SimpleSchema`) plus the Kubernetes resources that
//! implement it, wired together with CEL expressions (`${...}`). The engine
//! compiles the definition into a dependency graph, synthesizes the instance
//! CRD, and reconciles every instance by applying the templated children in
//! topological order, publishing observed child state back into the scope so
//! later nodes can reference it, and projecting child status onto the
//! instance.
//!
//! The engine rides one I/O trait — [`face::Face`] (list / watch / apply) —
//! so the same laws run standalone against an API server over HTTP
//! (`kro-kube`) and embedded in a host controller process over any medium
//! (`kro-kapi`).
//!
//! Module map (the laws live in the pure modules, the loops in [`engine`]):
//!
//! * [`api`] — the RGD surface parsed off the wire
//! * [`dag`] — stable topological ordering with cycle reporting
//! * [`simpleschema`] — `SimpleSchema` → structural `OpenAPI` v3
//! * [`expression`] — `${...}` scanning
//! * [`cel`] — the evaluation environment + the kro function library
//! * [`graph`] — compile: nodes, dependencies, validation, instance CRD
//! * [`crd`] — instance CRD synthesis
//! * [`conditions`] — the condition set law
//! * [`reconciler`] — one instance's apply/delete/status state machine
//! * [`engine`] — the dynamic loop set: pumps, dispatcher, workers, beat

pub mod api;
#[cfg(feature = "engine")]
pub mod cel;
#[cfg(feature = "engine")]
pub mod conditions;
#[cfg(feature = "engine")]
pub mod crd;
#[cfg(feature = "engine")]
pub mod dag;
pub mod expression;
#[cfg(feature = "engine")]
pub mod graph;
#[cfg(feature = "engine")]
pub mod labels;

#[cfg(feature = "controller")]
pub mod engine;
#[cfg(feature = "engine")]
pub mod face;
mod quantity;
#[cfg(feature = "controller")]
pub mod reconciler;
pub mod rgd_resources;
pub mod simpleschema;

pub use api::ResourceGraphDefinition;
#[cfg(feature = "controller")]
pub use engine::{Engine, EngineConfig};
#[cfg(feature = "controller")]
pub use face::{Action, Face, FaceError, Listed, SharedFace, Target, WatchEvent};
#[cfg(feature = "engine")]
pub use graph::{CompileError, CompiledGraph};
#[cfg(feature = "engine")]
pub use labels::{FINALIZER, LABEL_PREFIX, MANAGED_BY_KEY, MANAGED_BY_VALUE};
