//! The Function-as-RGD law (`KAPI-FR-115`): a Function is a PURE RGD —
//! no custom controller. The manifest at manifests/rgd/function.yaml declares
//! the instance schema, the Service + `HTTPRoute` + Deployment graph, and
//! the status projection. This test proves it parses, compiles and names
//! the right instance kind and resources.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use krapi::api;
use krapi::graph::{self, EngineDefaults};

const FUNCTION_RGD: &str = include_str!("../../../manifests/rgd/function.yaml");

fn parse_rgd() -> api::ResourceGraphDefinition {
    let body: serde_json::Value =
        serde_yaml::from_str(FUNCTION_RGD).expect("Function RGD is valid YAML");
    api::parse(&body, "kro.run").expect("Function RGD parses")
}

#[test]
fn function_rgd_parses_and_compiles() {
    let rgd = parse_rgd();
    assert_eq!(rgd.name, "function");
    assert_eq!(rgd.spec.schema.kind, "Function");
    assert_eq!(rgd.spec.schema.plural.as_deref(), Some("functions"));
    let compiled = graph::compile(&rgd, &EngineDefaults::default()).expect("Function RGD compiles");
    assert_eq!(compiled.instance_kind, "Function");
    assert_eq!(compiled.instance_plural, "functions");
    assert_eq!(compiled.instance_group, "kapi.qreta.io");
    assert_eq!(compiled.instance_version, "v1alpha1");
    assert!(compiled.namespaced);
}

#[test]
fn function_rgd_declares_service_and_httproute() {
    let compiled = graph::compile(&parse_rgd(), &EngineDefaults::default()).unwrap();
    let kinds: Vec<&str> = compiled.nodes.iter().map(|n| n.kind.as_str()).collect();
    assert!(
        kinds.contains(&"Service"),
        "the service definition is in the graph"
    );
    assert!(
        kinds.contains(&"HTTPRoute"),
        "the gateway route is in the graph"
    );
    assert_eq!(
        compiled.nodes.len(),
        2,
        "RGD-exclusive: Service + Route only"
    );
    let route = compiled
        .nodes
        .iter()
        .find(|n| n.kind == "HTTPRoute")
        .unwrap();
    assert_eq!(route.include_when.len(), 1);
    assert_eq!(route.for_each.len(), 1);
}

#[test]
fn function_rgd_projects_status_from_resource_outputs() {
    let compiled = graph::compile(&parse_rgd(), &EngineDefaults::default()).unwrap();
    let fields: Vec<String> = compiled
        .instance_status
        .fields
        .iter()
        .map(|(path, _)| {
            path.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(".")
        })
        .collect();
    assert!(
        fields.iter().any(|f| f.contains("ready")),
        "status.ready is projected: {fields:?}"
    );
    assert!(
        fields.iter().any(|f| f.contains("reason")),
        "status.reason is projected: {fields:?}"
    );
}
