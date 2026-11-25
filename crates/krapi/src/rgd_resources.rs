//! Typed RGD resource builders — the `resources` half of `#[derive(Rgd)]`.
//! Each function returns `Vec<Resource>` built from Rust values (serialized
//! to JSON), so the macro emits a function path and rustc checks the types.

use crate::api::{ExternalRef, Resource};
use serde_json::json;

/// Function status projection (typed CEL expressions).
#[must_use]
pub fn function_status() -> ::std::collections::BTreeMap<String, serde_json::Value> {
    let mut s = ::std::collections::BTreeMap::new();
    s.insert(
        "ready".to_owned(),
        json!("${functionService.metadata.name != \"\" && size(functionRoute) > 0}"),
    );
    s.insert("reason".to_owned(), json!("${functionService.metadata.name != \"\" && size(functionRoute) > 0 ? \"Ready\" : \"NotReady\"}"));
    s
}

/// WorkerJs/WorkerPy status projection.
#[must_use]
pub fn worker_status() -> ::std::collections::BTreeMap<String, serde_json::Value> {
    let mut s = ::std::collections::BTreeMap::new();
    s.insert(
        "functionState".to_owned(),
        json!("${functionRef.status.state}"),
    );
    s
}
/// Function: Service + one `HTTPRoute` per declared route.
#[must_use]
pub fn function_resources() -> Vec<Resource> {
    vec![
        Resource {
            id: "functionService".to_owned(),
            order: 0,
            template: Some(json!({
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {
                    "name": "${schema.metadata.name}",
                    "namespace": "${schema.metadata.namespace}",
                },
                "spec": {
                    "selector": { "app": "${schema.metadata.name}" },
                    "ports": [{ "name": "http", "port": 80, "targetPort": 80, "protocol": "TCP" }],
                    "type": "ClusterIP",
                },
            })),
            external_ref: None,
            ready_when: Vec::new(),
            include_when: Vec::new(),
            for_each: Vec::new(),
        },
        Resource {
            id: "functionRoute".to_owned(),
            order: 1,
            template: Some(route_template("${schema.metadata.name}")),
            external_ref: None,
            ready_when: Vec::new(),
            include_when: vec!["${size(schema.spec.routes) > 0}".to_owned()],
            for_each: vec![("route".to_owned(), "${schema.spec.routes}".to_owned())],
        },
    ]
}

/// WorkerJs/WorkerPy: read-only Function ref + one `HTTPRoute` per route.
#[must_use]
pub fn worker_resources() -> Vec<Resource> {
    vec![
        Resource {
            id: "functionRef".to_owned(),
            order: 0,
            template: None,
            external_ref: Some(ExternalRef {
                api_version: "kapi.qreta.io/v1alpha1".to_owned(),
                kind: "Function".to_owned(),
                namespace: Some("default".to_owned()),
                name: Some("wisolate".to_owned()),
            }),
            ready_when: Vec::new(),
            include_when: Vec::new(),
            for_each: Vec::new(),
        },
        Resource {
            id: "route".to_owned(),
            order: 1,
            template: Some(route_template("${schema.spec.functionRef}")),
            external_ref: None,
            ready_when: Vec::new(),
            include_when: vec!["${size(schema.spec.routes) > 0}".to_owned()],
            for_each: vec![("route".to_owned(), "${schema.spec.routes}".to_owned())],
        },
    ]
}

fn route_template(backend: &str) -> serde_json::Value {
    json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "HTTPRoute",
        "metadata": {
            "name": "${schema.metadata.name}-${route.split('/')[0]}${route.split('/').size() > 1 && route.split('/')[1] != '' ? '-' + route.split('/')[1] : ''}",
            "namespace": "${schema.metadata.namespace}",
        },
        "spec": {
            "hostnames": ["${route.split('/')[0]}"],
            "rules": [{
                "matches": [{ "path": {
                    "type": "PathPrefix",
                    "value": "${route.split('/').size() > 1 && route.split('/')[1] != '' ? '/' + route.split('/')[1] + '/' : '/'}",
                }}],
                "backendRefs": [{ "name": backend, "port": 80 }],
            }],
        },
    })
}
