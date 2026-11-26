//! FunctionApp: the single app RGD that replaces WorkerJs/WorkerPy.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use krapi::{
    api,
    graph::{self, EngineDefaults},
};
const RGD: &str = include_str!("../../../../kapi/manifests/rgd/functionapp.yaml");
#[test]
fn functionapp_rgd_compiles() {
    let body: serde_json::Value = serde_yaml::from_str(RGD).expect("valid YAML");
    let rgd = api::parse(&body, "kro.run").expect("parses");
    assert_eq!(rgd.spec.schema.kind, "FunctionApp");
    let g = graph::compile(&rgd, &EngineDefaults::default()).expect("compiles");
    assert_eq!(g.instance_plural, "functionapps");
    assert_eq!(g.nodes.len(), 2);
    assert_eq!(g.instance_status.fields.len(), 2);
}
