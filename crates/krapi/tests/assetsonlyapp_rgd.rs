//! A definition-only RGD (AssetsOnlyApp) compiles with zero resources.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use krapi::{api, graph::{self, EngineDefaults}};

const RGD: &str = include_str!("../../../../kapi/manifests/rgd/assetsonlyapp.yaml");

#[test]
fn empty_resource_rgd_compiles() {
    let body: serde_json::Value = serde_yaml::from_str(RGD).expect("valid YAML");
    let rgd = api::parse(&body, "kro.run").expect("empty-resource RGD parses");
    assert_eq!(rgd.spec.schema.kind, "AssetsOnlyApp");
    let g = graph::compile(&rgd, &EngineDefaults::default()).expect("compiles with zero nodes");
    assert!(g.nodes.is_empty());
    assert_eq!(g.instance_plural, "assetsonlyapps");
}