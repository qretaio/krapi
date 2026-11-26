//! Runtime check: the Function RGD splits a `host/path` route into the
//! `HTTPRoute` hostname + pathPrefix.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use krapi::api;
use krapi::cel::{self, Scope};
use krapi::graph::{self, EngineDefaults};
use serde_json::json;

const FUNCTION_RGD: &str = include_str!("../../../manifests/rgd/function.yaml");

#[test]
fn function_route_splits_host_and_path() {
    let body: serde_json::Value = serde_yaml::from_str(FUNCTION_RGD).unwrap();
    let rgd = api::parse(&body, "kro.run").unwrap();
    let compiled = graph::compile(&rgd, &EngineDefaults::default()).unwrap();
    let route = compiled
        .nodes
        .iter()
        .find(|n| n.id == "functionRoute")
        .unwrap();
    let tpl = serde_json::to_string(&route.template).unwrap();
    assert!(
        tpl.contains("split"),
        "route template splits host/path: {tpl}"
    );
    let mut scope = Scope::new();
    scope.insert("route".to_owned(), json!("qreta.io/games/"));
    assert_eq!(
        cel::eval_value("route.split(\"/\")[0]", &scope).unwrap(),
        json!("qreta.io")
    );
}
