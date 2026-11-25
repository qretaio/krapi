//! THE SIMPLESCHEMA → OPENAPI CONVERSION — `replicas: integer |
//! default=1 minimum=1 maximum=10` becomes a structural `OpenAPI` v3
//! schema. The output deliberately stays inside the structural subset
//! every Kubernetes-shaped API server enforces on CRDs (type,
//! properties, items, required, enum, default, bounds, the supported
//! pattern subset, preserve-unknown), so the synthesized instance CRD
//! applies unchanged anywhere — including kapi's `is_structural` gate.
//!
//! Custom `types` resolve through ONE shared resolver (nested objects
//! never re-enter the constructor); a cycle is a refusal.

use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};

/// Convert a `SimpleSchema` body (a map of field → type string or nested
/// map) into the `OpenAPI` schema object.
///
/// # Errors
/// An unknown type, a bad marker, an unknown custom type reference, or a
/// cycle in the custom types.
#[allow(clippy::too_many_lines)] // the marker walk reads best in one pass
pub fn to_openapi(
    body: &BTreeMap<String, Json>,
    types: &BTreeMap<String, Json>,
) -> Result<Json, String> {
    let resolver = Resolver::new(types)?;
    build_object(body, &resolver)
}

/// Build one object schema from a field map.
fn build_object(body: &BTreeMap<String, Json>, r: &Resolver) -> Result<Json, String> {
    let mut required = Vec::new();
    let mut properties = serde_json::Map::new();
    for (name, field) in body {
        let schema = field_schema(name, field, r, &mut required)?;
        properties.insert(name.clone(), schema);
    }
    required.sort();
    let mut out = json!({"type": "object"});
    if !properties.is_empty() {
        out["properties"] = Json::Object(properties);
    }
    if !required.is_empty() {
        out["required"] = json!(required);
    }
    Ok(out)
}

/// The shared custom-type resolver: `built` caches finished schemas,
/// `building` guards re-entry (the cycle refusal).
struct Resolver {
    types: BTreeMap<String, Json>,
    building: std::cell::RefCell<BTreeSet<String>>,
    built: std::cell::RefCell<BTreeMap<String, Json>>,
}

impl Resolver {
    fn new(types: &BTreeMap<String, Json>) -> Result<Self, String> {
        let r = Self {
            types: types.clone(),
            building: std::cell::RefCell::new(BTreeSet::new()),
            built: std::cell::RefCell::new(BTreeMap::new()),
        };
        // Build every type now so cycles surface at compile time, not at
        // the first reference.
        for name in types.keys() {
            r.resolve(name)?;
        }
        Ok(r)
    }

    fn resolve(&self, name: &str) -> Result<Json, String> {
        if let Some(b) = self.built.borrow().get(name) {
            return Ok(b.clone());
        }
        if self.building.borrow().contains(name) {
            return Err(format!("cyclic custom type {name:?}"));
        }
        let body = self
            .types
            .get(name)
            .ok_or_else(|| format!("unknown custom type {name:?}"))?;
        self.building.borrow_mut().insert(name.to_owned());
        let mut required = Vec::new();
        let result = match body {
            Json::String(s) => {
                let (ty, markers) = parse_field(s)?;
                let mut sch = type_schema(&ty, self)?;
                apply_markers(&mut sch, &markers, name, &mut required)?;
                Ok(sch)
            }
            Json::Object(_) => {
                let body_map: BTreeMap<String, Json> = body
                    .as_object()
                    .map_or_default(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
                build_object(&body_map, self)
            }
            other => Err(format!(
                "custom type {name:?} must be a string or object, got: {other}"
            )),
        };
        self.building.borrow_mut().remove(name);
        if let Ok(schema) = &result {
            self.built
                .borrow_mut()
                .insert(name.to_owned(), schema.clone());
        }
        result
    }
}

fn field_schema(
    name: &str,
    field: &Json,
    r: &Resolver,
    required: &mut Vec<String>,
) -> Result<Json, String> {
    match field {
        Json::String(s) => {
            let (ty, markers) = parse_field(s)?;
            // a custom type-alias's own required=true marker propagates
            if is_custom(&ty)
                && r.types.get(&ty).and_then(Json::as_str).is_some_and(|s| {
                    s.split('|')
                        .nth(1)
                        .is_some_and(|m| m.split_whitespace().any(|tok| tok == "required=true"))
                })
            {
                required.push(name.to_owned());
            }
            let mut sch = type_schema(&ty, r)?;
            apply_markers(&mut sch, &markers, name, required)?;
            Ok(sch)
        }
        Json::Object(_) => {
            let body_map: BTreeMap<String, Json> = field
                .as_object()
                .map_or_default(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
            build_object(&body_map, r)
        }
        other => Err(format!(
            "field {name:?} must be a type string or an object, got: {other}"
        )),
    }
}

/// Atomic type names; everything else is a custom type reference.
fn is_atomic(ty: &str) -> bool {
    matches!(ty, "string" | "integer" | "number" | "boolean" | "object")
}

fn is_custom(ty: &str) -> bool {
    !is_atomic(ty) && !ty.starts_with("[]") && !ty.starts_with("map[string]") && ty != "array"
}

fn type_schema(ty: &str, r: &Resolver) -> Result<Json, String> {
    if let Some(elem) = ty.strip_prefix("[]") {
        return Ok(json!({
            "type": "array",
            "items": type_schema(elem, r)?
        }));
    }
    if let Some(val) = ty.strip_prefix("map[string]") {
        return Ok(json!({
            "type": "object",
            "additionalProperties": type_schema(val, r)?
        }));
    }
    match ty {
        "string" => Ok(json!({"type": "string"})),
        "integer" => Ok(json!({"type": "integer", "format": "int64"})),
        "number" => Ok(json!({"type": "number"})),
        "boolean" => Ok(json!({"type": "boolean"})),
        "object" => Ok(json!({"type": "object", "x-kubernetes-preserve-unknown-fields": true})),
        other => r.resolve(other),
    }
}

/// One parsed marker (`required=true`, `default="x"`, ...).
#[derive(Debug, Clone)]
struct Marker {
    name: String,
    value: String,
}

/// Parse `type | marker=value marker=value` (the part after the first `|`).
fn parse_field(s: &str) -> Result<(String, Vec<Marker>), String> {
    let mut parts = s.splitn(2, '|');
    let ty = parts.next().unwrap_or_default().trim().to_owned();
    if ty.is_empty() {
        return Err(format!("empty type in field: {s:?}"));
    }
    let mut markers = Vec::new();
    if let Some(marker_str) = parts.next() {
        markers = parse_markers(marker_str)?;
    }
    Ok((ty, markers))
}

/// Parse `key=value` markers, quote- and bracket-aware (a marker value
/// may carry spaces inside quotes or brackets).
fn parse_markers(s: &str) -> Result<Vec<Marker>, String> {
    let mut out: Vec<Marker> = Vec::new();
    let mut key = String::new();
    let mut value = String::new();
    let mut in_key = true;
    let mut in_quotes = false;
    let mut escaped = false;
    let mut depth = 0i32;
    let mut pending = false;
    for c in s.chars() {
        if in_key {
            match c {
                '=' => {
                    in_key = false;
                    pending = true;
                }
                c if c.is_whitespace() => {
                    if !key.is_empty() {
                        return Err(format!("marker key {key:?} without a value"));
                    }
                }
                _ => key.push(c),
            }
            continue;
        }
        match c {
            '"' if !escaped => {
                in_quotes = !in_quotes;
                value.push(c);
            }
            '\\' if in_quotes => {
                escaped = true;
                value.push(c);
            }
            '{' | '[' if !in_quotes => {
                depth += 1;
                value.push(c);
            }
            '}' | ']' if !in_quotes => {
                depth -= 1;
                value.push(c);
            }
            c if c.is_whitespace() && !in_quotes && depth == 0 => {
                flush_marker(&mut out, &mut key, &mut value);
                in_key = true;
                pending = false;
            }
            _ => {
                value.push(c);
            }
        }
    }
    if pending && value.is_empty() {
        return Err(format!("marker key {key:?} without a value"));
    }
    if in_quotes {
        return Err(format!("unclosed quote in markers: {s:?}"));
    }
    if depth != 0 {
        return Err(format!("unclosed bracket in markers: {s:?}"));
    }
    flush_marker(&mut out, &mut key, &mut value);
    // validate the marker names up front
    for m in &out {
        if !KNOWN_MARKERS.contains(&m.name.as_str()) {
            return Err(format!("unknown marker {m:?}"));
        }
    }
    Ok(out)
}

fn flush_marker(out: &mut Vec<Marker>, key: &mut String, value: &mut String) {
    if !key.is_empty() {
        out.push(Marker {
            name: std::mem::take(key),
            value: std::mem::take(value),
        });
    }
}

const KNOWN_MARKERS: &[&str] = &[
    "required",
    "default",
    "description",
    "minimum",
    "maximum",
    "validation",
    "enum",
    "immutable",
    "pattern",
    "uniqueItems",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "listType",
    "listMapKey",
];

#[allow(clippy::too_many_lines)] // the marker walk reads best in one pass
fn apply_markers(
    schema: &mut Json,
    markers: &[Marker],
    name: &str,
    parent_required: &mut Vec<String>,
) -> Result<(), String> {
    for m in markers {
        let val = strip_outer_quotes(&m.value);
        match m.name.as_str() {
            "required" => {
                if val == "true" {
                    parent_required.push(name.to_owned());
                }
            }
            "default" => {
                let ty = schema
                    .get("type")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                let default = match ty {
                    "string" => json!(val),
                    "integer" => Json::Number(
                        val.parse::<i64>()
                            .map_err(|_| {
                                format!("field {name}: default {val:?} is not an integer")
                            })?
                            .into(),
                    ),
                    "number" => Json::Number(
                        serde_json::Number::from_f64(val.parse::<f64>().map_err(|_| {
                            format!("field {name}: default {val:?} is not a number")
                        })?)
                        .ok_or_else(|| {
                            format!("field {name}: default {val:?} is not a finite number")
                        })?,
                    ),
                    "boolean" => json!(val == "true"),
                    _ => serde_json::from_str(&val)
                        .map_err(|e| format!("field {name}: default is not valid JSON: {e}"))?,
                };
                schema["default"] = default;
            }
            "description" => schema["description"] = json!(val),
            "minimum" => schema["minimum"] = number_marker(name, &m.value, "minimum")?,
            "maximum" => schema["maximum"] = number_marker(name, &m.value, "maximum")?,
            "enum" => {
                let ty = schema
                    .get("type")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                let mut items = Vec::new();
                for v in val.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                    items.push(match ty {
                        "integer" => Json::Number(
                            v.parse::<i64>()
                                .map_err(|_| {
                                    format!("field {name}: enum value {v:?} is not an integer")
                                })?
                                .into(),
                        ),
                        _ => json!(v),
                    });
                }
                schema["enum"] = json!(items);
            }
            "immutable" => {
                if val == "true" {
                    push_rule(
                        schema,
                        json!({"rule": "self == oldSelf", "message": "field is immutable"}),
                    );
                }
            }
            "validation" => {
                push_rule(schema, json!({"rule": val, "message": "validation failed"}));
            }
            "pattern" => {
                if schema.get("type").and_then(Json::as_str) != Some("string") {
                    return Err(format!(
                        "field {name}: the pattern marker only applies to strings"
                    ));
                }
                schema["pattern"] = json!(val);
            }
            "minLength" => schema["minLength"] = int_marker(name, &m.value, "minLength")?,
            "maxLength" => schema["maxLength"] = int_marker(name, &m.value, "maxLength")?,
            "minItems" => {
                expect_array(schema, name)?;
                schema["minItems"] = int_marker(name, &m.value, "minItems")?;
            }
            "maxItems" => {
                expect_array(schema, name)?;
                schema["maxItems"] = int_marker(name, &m.value, "maxItems")?;
            }
            "uniqueItems" => {
                expect_array(schema, name)?;
                if val == "true" {
                    schema["x-kubernetes-list-type"] = json!("set");
                }
            }
            "listType" => {
                expect_array(schema, name)?;
                if !matches!(val.as_str(), "atomic" | "set" | "map") {
                    return Err(format!(
                        "field {name}: listType {val:?} must be atomic, set, or map"
                    ));
                }
                schema["x-kubernetes-list-type"] = json!(val);
            }
            "listMapKey" => {
                expect_array(schema, name)?;
                if schema.get("x-kubernetes-list-map-keys").is_none() {
                    schema["x-kubernetes-list-map-keys"] = json!([]);
                }
                if let Some(keys) = schema
                    .get_mut("x-kubernetes-list-map-keys")
                    .and_then(Json::as_array_mut)
                {
                    keys.push(json!(val));
                }
            }
            other => return Err(format!("unknown marker {other:?}")),
        }
    }
    Ok(())
}

fn push_rule(schema: &mut Json, rule: Json) {
    if schema.get("x-kubernetes-validations").is_none() {
        schema["x-kubernetes-validations"] = json!([]);
    }
    if let Some(rules) = schema
        .get_mut("x-kubernetes-validations")
        .and_then(Json::as_array_mut)
    {
        rules.push(rule);
    }
}

fn expect_array(schema: &Json, name: &str) -> Result<(), String> {
    if schema.get("type").and_then(Json::as_str) != Some("array") {
        return Err(format!("field {name}: that marker only applies to arrays"));
    }
    Ok(())
}

fn number_marker(name: &str, v: &str, what: &str) -> Result<Json, String> {
    let f: f64 = v
        .parse()
        .map_err(|_| format!("field {name}: {what} {v:?} is not a number"))?;
    Ok(json!(f))
}

fn int_marker(name: &str, v: &str, what: &str) -> Result<Json, String> {
    let i: i64 = v
        .parse()
        .map_err(|_| format!("field {name}: {what} {v:?} is not an integer"))?;
    Ok(json!(i))
}

fn strip_outer_quotes(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        return inner.replace("\\\"", "\"").replace("\\\\", "\\");
    }
    s.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn body(v: &Json) -> BTreeMap<String, Json> {
        v.as_object()
            .expect("object")
            .iter()
            .map(|(k, x)| (k.clone(), x.clone()))
            .collect()
    }

    #[test]
    fn scalars_defaults_and_markers() {
        let out = to_openapi(
            &body(&json!({
                "name": "string | required=true description=\"the name\" minLength=1 maxLength=8",
                "replicas": "integer | default=1 minimum=1 maximum=10",
                "env": "string | default=dev enum=dev,staging,prod",
                "tls": "boolean | default=false"
            })),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out["type"], json!("object"));
        assert_eq!(out["required"], json!(["name"]));
        assert_eq!(out["properties"]["replicas"]["default"], json!(1));
        assert_eq!(out["properties"]["name"]["description"], json!("the name"));
        assert_eq!(
            out["properties"]["env"]["enum"],
            json!(["dev", "staging", "prod"])
        );
        assert_eq!(out["properties"]["tls"]["default"], json!(false));
        assert_eq!(out["properties"]["replicas"]["minimum"], json!(1.0));
        assert_eq!(out["properties"]["replicas"]["maximum"], json!(10.0));
        assert_eq!(out["properties"]["name"]["minLength"], json!(1));
        assert_eq!(out["properties"]["name"]["maxLength"], json!(8));
    }

    #[test]
    fn arrays_maps_and_nested_objects() {
        let out = to_openapi(
            &body(&json!({
                "regions": "[]string | minItems=1",
                "labels": "map[string]string",
                "server": {"host": "string | required=true", "port": "integer | default=8080"},
                "extra": "object"
            })),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out["properties"]["regions"]["type"], json!("array"));
        assert_eq!(
            out["properties"]["regions"]["items"]["type"],
            json!("string")
        );
        assert_eq!(
            out["properties"]["labels"]["additionalProperties"]["type"],
            json!("string")
        );
        assert_eq!(
            out["properties"]["server"]["properties"]["port"]["default"],
            json!(8080)
        );
        assert_eq!(out["properties"]["server"]["required"], json!(["host"]));
        assert_eq!(
            out["properties"]["extra"]["x-kubernetes-preserve-unknown-fields"],
            json!(true)
        );
    }

    #[test]
    fn custom_types_resolve_and_cycle_refuses() {
        let types = body(&json!({
            "Server": {"host": "string | required=true", "port": "integer | default=80"},
            "Cluster": {"primary": "Server | required=true", "name": "string"}
        }));
        let out = to_openapi(&body(&json!({"cluster": "Cluster"})), &types).unwrap();
        assert_eq!(
            out["properties"]["cluster"]["properties"]["primary"]["properties"]["port"]["default"],
            json!(80)
        );
        assert_eq!(out["properties"]["cluster"]["required"], json!(["primary"]));

        let cyclic = body(&json!({"A": {"b": "B"}, "B": {"a": "A"}}));
        assert!(to_openapi(&body(&json!({"x": "A"})), &cyclic).is_err());
        let unknown = body(&json!({"Missing": {"a": "Nope"}}));
        assert!(to_openapi(&body(&json!({"x": "Missing"})), &unknown).is_err());
    }

    #[test]
    fn immutable_and_validation_markers_become_cel_rules() {
        let out = to_openapi(
            &body(&json!({
                "name": "string | immutable=true validation=\"self.size() > 0\""
            })),
            &BTreeMap::new(),
        )
        .unwrap();
        let rules = out["properties"]["name"]["x-kubernetes-validations"]
            .as_array()
            .expect("rules");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["rule"], json!("self == oldSelf"));
        assert_eq!(rules[1]["rule"], json!("self.size() > 0"));
    }

    #[test]
    fn unknown_marker_refuses() {
        assert!(to_openapi(&body(&json!({"x": "string | bogus=1"})), &BTreeMap::new()).is_err());
    }

    #[test]
    fn quoted_marker_values_carry_spaces() {
        let out = to_openapi(
            &body(&json!({"name": "string | description=\"a b c\" default=\"x y\""})),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out["properties"]["name"]["description"], json!("a b c"));
        assert_eq!(out["properties"]["name"]["default"], json!("x y"));
    }
}
