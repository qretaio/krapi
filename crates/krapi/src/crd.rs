//! THE INSTANCE CRD SYNTHESIS — one RGD in, one `CustomResourceDefinition`
//! body out. The shape is the upstream one (`<plural>.<group>`, one
//! version, status subresource) with a structural schema every
//! Kubernetes-shaped API server accepts: the SimpleSchema-derived spec,
//! the typed status (state + conditions + author fields), and the
//! metadata.name validation markers when the RGD carries them.

use serde_json::{Value as Json, json};

/// Synthesize the instance CRD body.
#[must_use]
#[allow(clippy::too_many_arguments)] // the synthesis carries the CRD's whole shape
pub fn synthesize(
    group: &str,
    version: &str,
    kind: &str,
    plural: &str,
    spec_schema: &Json,
    status_schema: &Json,
    namespaced: bool,
    name_validation: Option<&str>,
) -> Json {
    let mut metadata_schema = json!({"type": "object"});
    if let Some(markers) = name_validation
        && let Ok(m) = crate::simpleschema::to_openapi(
            &std::iter::once((
                "name".to_owned(),
                Json::String(format!("string | {markers}")),
            ))
            .collect(),
            &std::collections::BTreeMap::new(),
        )
        && let Some(name_schema) = m.pointer("/properties/name").cloned()
    {
        metadata_schema["properties"] = json!({"name": name_schema});
    }

    json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": format!("{plural}.{group}")},
        "spec": {
            "group": group,
            "scope": if namespaced { "Namespaced" } else { "Cluster" },
            "names": {
                "plural": plural,
                "singular": kind.to_ascii_lowercase(),
                "kind": kind,
                "listKind": format!("{kind}List"),
            },
            "versions": [{
                "name": version,
                "served": true,
                "storage": true,
                "subresources": {"status": {}},
                "schema": {
                    "openAPIV3Schema": {
                        "type": "object",
                        "properties": {
                            "apiVersion": {"type": "string"},
                            "kind": {"type": "string"},
                            "metadata": metadata_schema,
                            "spec": spec_schema.clone(),
                            "status": status_schema.clone(),
                        }
                    }
                }
            }]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn the_crd_shape_is_complete() {
        let spec = crate::simpleschema::to_openapi(
            &std::iter::once(("name".to_owned(), json!("string | required=true")))
                .collect::<std::collections::BTreeMap<String, Json>>(),
            &BTreeMap::new(),
        )
        .unwrap();
        let status = json!({"type": "object", "properties": {
            "state": {"type": "string"}
        }});
        let crd = synthesize(
            "kro.rs",
            "v1alpha1",
            "WebStore",
            "webstores",
            &spec,
            &status,
            true,
            None,
        );
        assert_eq!(crd["apiVersion"], json!("apiextensions.k8s.io/v1"));
        assert_eq!(crd["kind"], json!("CustomResourceDefinition"));
        assert_eq!(crd["metadata"]["name"], json!("webstores.kro.rs"));
        assert_eq!(crd["spec"]["group"], json!("kro.rs"));
        assert_eq!(crd["spec"]["scope"], json!("Namespaced"));
        assert_eq!(crd["spec"]["names"]["singular"], json!("webstore"));
        assert_eq!(crd["spec"]["names"]["listKind"], json!("WebStoreList"));
        let v = &crd["spec"]["versions"][0];
        assert_eq!(v["name"], json!("v1alpha1"));
        assert_eq!(v["served"], json!(true));
        assert_eq!(v["storage"], json!(true));
        assert_eq!(v["subresources"]["status"], json!({}));
        let schema = &v["schema"]["openAPIV3Schema"];
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["properties"]["kind"], json!({"type": "string"}));
        assert_eq!(
            schema["properties"]["spec"]["properties"]["name"]["type"],
            json!("string")
        );
        assert_eq!(
            schema["properties"]["status"]["properties"]["state"]["type"],
            json!("string")
        );
    }

    #[test]
    fn cluster_scope_and_name_validation_carry_through() {
        let spec = json!({"type": "object"});
        let status = json!({"type": "object"});
        let crd = synthesize(
            "kro.rs",
            "v1alpha1",
            "Zone",
            "zones",
            &spec,
            &status,
            false,
            Some("maxLength=30 pattern=^[a-z].*$"),
        );
        assert_eq!(crd["spec"]["scope"], json!("Cluster"));
        let name = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["metadata"]
            ["properties"]["name"];
        assert_eq!(name["maxLength"], json!(30));
        assert_eq!(name["pattern"], json!("^[a-z].*$"));
    }
}
