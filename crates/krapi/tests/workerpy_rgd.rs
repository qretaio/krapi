//! The WorkerPy-as-RGD law: a `WorkerPy` REUSES an existing Function via
//! `functionRef` and only adds `HTTPRoute`s pointing at that Service.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use krapi::api;
use krapi::graph::{self, EngineDefaults};

const WORKERJS_RGD: &str = include_str!("../../../manifests/rgd/workerpy.yaml");

#[test]
fn workerpy_reuses_a_function_and_routes_to_it() {
    let body: serde_json::Value = serde_yaml::from_str(WORKERJS_RGD).unwrap();
    let rgd = api::parse(&body, "kro.run").unwrap();
    let compiled = graph::compile(&rgd, &EngineDefaults::default()).unwrap();
    assert_eq!(compiled.instance_kind, "WorkerPy");
    assert_eq!(compiled.instance_plural, "workerpyapps");
    assert_eq!(compiled.nodes.len(), 2);
    assert_eq!(compiled.topological_order, ["functionRef", "route"]);
    let shared = compiled.nodes.first().unwrap();
    assert!(shared.external, "functionRef is a read-only externalRef");
    assert_eq!(shared.kind, "Function");
    let route = compiled.nodes.last().unwrap();
    let tpl = serde_json::to_string(&route.template).unwrap();
    assert!(
        tpl.contains("functionRef"),
        "backendRef uses functionRef: {tpl}"
    );
}
