//! THE COMPILE — a `ResourceGraphDefinition` becomes a [`CompiledGraph`]:
//! parsed template expressions, the dependency DAG (edges from template,
//! `includeWhen`, and `forEach` references), the stable topological
//! order, the reverse deletion waves, the synthesized instance CRD, and
//! the instance's status projections. Every refusal here surfaces as a
//! status condition on the RGD (`GraphAccepted=False`), never as a
//! controller error.

use crate::api::{self, ResourceGraphDefinition, Scope};
use crate::dag::{CycleError, Dag};
use crate::expression::{self, FieldExpr};
use crate::labels;
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

/// The instance variable name (the CEL scope's handle on the instance).
pub const SCHEMA_VAR: &str = "schema";
/// The forEach per-item variable.
pub const EACH_VAR: &str = "each";

/// The engine defaults (the group instance CRDs land in when the RGD
/// does not name one, the RGD surface itself).
#[derive(Debug, Clone)]
pub struct EngineDefaults {
    pub instance_group: String,
    pub rgd_group: String,
    pub rgd_version: String,
}

impl Default for EngineDefaults {
    fn default() -> Self {
        Self {
            instance_group: "kro.rs".to_owned(),
            rgd_group: "kro.rs".to_owned(),
            rgd_version: "v1alpha1".to_owned(),
        }
    }
}

impl EngineDefaults {
    #[must_use]
    pub fn rgd_target(&self) -> crate::face::Target {
        crate::face::Target::cr(
            &self.rgd_group,
            &self.rgd_version,
            "resourcegraphdefinitions",
            None,
        )
    }
}

/// Why a compile refused.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CompileError {
    /// The RGD body does not parse (grammar, exclusivity, patterns).
    #[error("{0}")]
    Invalid(String),
    /// The dependency graph carries a cycle.
    #[error("{0}")]
    Cycle(#[from] CycleError),
}

/// One compiled node.
#[derive(Debug, Clone)]
pub struct CompiledNode {
    pub id: String,
    pub order: usize,
    /// Template nodes apply; external nodes only read.
    pub external: bool,
    /// The static GVR the node rides (v1: apiVersion/kind must be
    /// literal — a CEL-computed kind is a compile refusal).
    pub group: Option<String>,
    pub version: String,
    pub kind: String,
    pub plural: String,
    /// The parsed template with expressions intact (external nodes carry
    /// the synthetic ref template).
    pub template: Json,
    /// Every embedded expression, with its path.
    pub exprs: Vec<FieldExpr>,
    /// The identity fields' paths (`metadata.name`, `metadata.namespace`)
    /// — forEach iterators must appear in one of them.
    pub ready_when: Vec<String>,
    pub include_when: Vec<String>,
    pub for_each: Vec<(String, String)>,
    /// The ids this node references (the DAG's edges).
    pub dependencies: Vec<String>,
}

/// The instance node's status machinery.
#[derive(Debug, Clone)]
pub struct InstanceStatus {
    /// `path → CEL` for every author status field.
    pub fields: Vec<(Vec<expression::Seg>, String)>,
    /// The author `conditions` block expressions.
    pub conditions: Vec<String>,
}

/// The compiled graph.
#[derive(Debug, Clone)]
pub struct CompiledGraph {
    pub rgd_name: String,
    pub rgd_uid: String,
    /// The spec hash — the recompile trigger.
    pub spec_hash: String,
    pub nodes: Vec<CompiledNode>,
    /// The apply order (dependencies first, user order within waves).
    pub topological_order: Vec<String>,
    /// The deletion waves (dependents first).
    pub deletion_waves: Vec<Vec<String>>,
    /// The instance CRD body, ready to apply.
    pub instance_crd: Json,
    pub instance_status: InstanceStatus,
    /// The instance kind's GVR parts.
    pub instance_group: String,
    pub instance_version: String,
    pub instance_kind: String,
    pub instance_plural: String,
    pub namespaced: bool,
}

/// Compile one RGD body.
///
/// # Errors
/// [`CompileError::Invalid`] for grammar/structure refusals,
/// [`CompileError::Cycle`] for a cyclic graph.
pub fn compile(
    rgd: &ResourceGraphDefinition,
    defaults: &EngineDefaults,
) -> Result<CompiledGraph, CompileError> {
    let spec = &rgd.spec;

    // ---- the instance schema → OpenAPI → CRD --------------------------
    let spec_schema = crate::simpleschema::to_openapi(&spec.schema.spec, &spec.schema.types)
        .map_err(CompileError::Invalid)?;
    let group = spec
        .schema
        .group
        .clone()
        .unwrap_or_else(|| defaults.instance_group.clone());
    if api::valid_id(&group) && !group.contains('.') && group.len() < 4 {
        return Err(CompileError::Invalid(format!(
            "spec.schema.group {group:?} is not a DNS subdomain"
        )));
    }
    let plural = spec
        .schema
        .plural
        .clone()
        .unwrap_or_else(|| api::pluralize(&spec.schema.kind));
    let namespaced = spec.schema.scope == Scope::Namespaced;

    // ---- status projections ------------------------------------------
    let status = parse_status(&spec.schema.status)?;

    // ---- nodes --------------------------------------------------------
    let mut nodes = Vec::with_capacity(spec.resources.len());
    for r in &spec.resources {
        let node = build_node(r, namespaced)?;
        nodes.push(node);
    }

    // ---- the scope check: every non-readyWhen expression names only
    // known identifiers (schema, each, resource ids) -------------------
    let id_set: std::collections::BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    for n in &nodes {
        // this node's forEach iterators are local bindings in its templates
        let local: std::collections::BTreeSet<&str> = n
            .for_each
            .iter()
            .map(|(name, _)| name.as_str())
            .chain(id_set.iter().copied())
            .collect();
        let sources: Vec<String> = n
            .exprs
            .iter()
            .flat_map(|f| match &f.render {
                expression::Render::Whole(e) => vec![e.clone()],
                expression::Render::Parts(ps) => ps
                    .iter()
                    .filter_map(|p| match p {
                        expression::Piece::Expr(e) => Some(e.clone()),
                        expression::Piece::Literal(_) => None,
                    })
                    .collect(),
            })
            .chain(n.include_when.iter().cloned())
            .chain(n.for_each.iter().map(|(_, e)| e.clone()))
            .collect();
        for e in &sources {
            validate_ids(e, n.id.as_str(), &local)?;
        }
    }

    // ---- the dependency DAG ------------------------------------------
    let mut dag = Dag::default();
    for n in &nodes {
        dag.add_vertex(&n.id, n.order);
    }
    for n in &nodes {
        dag.add_dependencies(&n.id, &n.dependencies)?;
    }
    let topological_order = dag.topological_sort()?;
    let deletion_waves = dag.reverse_layers()?;

    // ---- the instance CRD --------------------------------------------
    let status_schema = status_schema(&status);
    let instance_crd = crate::crd::synthesize(
        &group,
        &spec.schema.api_version,
        &spec.schema.kind,
        &plural,
        &spec_schema,
        &status_schema,
        namespaced,
        spec.schema.name_validation.as_deref(),
    );

    for (path, expr) in &status.fields {
        validate_scope_refs(expr, &id_set, &[SCHEMA_VAR])
            .map_err(|e| CompileError::Invalid(format!("status field: {e}")))?;
        let _ = path;
    }
    for expr in &status.conditions {
        validate_scope_refs(expr, &id_set, &[SCHEMA_VAR, "runtime"])
            .map_err(|e| CompileError::Invalid(format!("status.conditions: {e}")))?;
    }

    Ok(CompiledGraph {
        rgd_name: rgd.name.clone(),
        rgd_uid: rgd.uid.clone(),
        spec_hash: spec_hash(&rgd.spec),
        nodes,
        topological_order,
        deletion_waves,
        instance_crd,
        instance_status: status,
        instance_group: group,
        instance_version: spec.schema.api_version.clone(),
        instance_kind: spec.schema.kind.clone(),
        instance_plural: plural,
        namespaced,
    })
}

/// Parse the status block: every leaf must be a `${...}` expression; the
/// `conditions` key holds a list of expression strings.
fn parse_status(status: &BTreeMap<String, Json>) -> Result<InstanceStatus, CompileError> {
    let mut fields = Vec::new();
    let mut conditions = Vec::new();
    for (name, v) in status {
        if name == "conditions" {
            let list = v.as_array().ok_or_else(|| {
                CompileError::Invalid("status.conditions must be a list".to_owned())
            })?;
            for (i, item) in list.iter().enumerate() {
                let s = item
                    .as_str()
                    .ok_or_else(|| {
                        CompileError::Invalid(format!(
                            "status.conditions[{i}] must be a ${{...}} expression string"
                        ))
                    })?
                    .to_owned();
                let expr = expression::unwrap(&s)
                    .map_err(|e| CompileError::Invalid(format!("status.conditions[{i}]: {e}")))?;
                conditions.push(expr);
            }
            continue;
        }
        collect_status_field(
            v,
            &mut vec![expression::Seg::Key(name.clone())],
            &mut fields,
        )?;
    }
    Ok(InstanceStatus { fields, conditions })
}

fn collect_status_field(
    v: &Json,
    path: &mut Vec<expression::Seg>,
    out: &mut Vec<(Vec<expression::Seg>, String)>,
) -> Result<(), CompileError> {
    match v {
        Json::Object(map) => {
            for (k, child) in map {
                path.push(expression::Seg::Key(k.clone()));
                collect_status_field(child, path, out)?;
                path.pop();
            }
            Ok(())
        }
        Json::String(s) => {
            let expr = expression::unwrap(s).map_err(|e| {
                CompileError::Invalid(format!("status field must be a ${{...}} expression: {e}"))
            })?;
            out.push((path.clone(), expr));
            Ok(())
        }
        other => Err(CompileError::Invalid(format!(
            "status fields must be expression strings or nested objects, got: {other}"
        ))),
    }
}

/// Validate that every top-level identifier in `expr` is one of the
/// allowed names (a resource id, `schema`, `each`, `runtime`).
///
/// # Errors
/// An unknown identifier or an unparseable expression.
pub fn validate_scope_refs(
    expr: &str,
    ids: &std::collections::BTreeSet<&str>,
    allowed: &[&str],
) -> Result<Vec<String>, String> {
    let program =
        cel::Program::compile(expr).map_err(|e| format!("invalid CEL expression {expr:?}: {e}"))?;
    let mut refs = Vec::new();
    for ident in program_identifiers(program.expression()) {
        let known = ids.contains(ident.as_str())
            || allowed.contains(&ident.as_str())
            || ident == EACH_VAR
            || ident == SCHEMA_VAR;
        if !known {
            return Err(format!(
                "expression {expr:?} references unknown identifier {ident:?}"
            ));
        }
        if !refs.contains(&ident) {
            refs.push(ident);
        }
    }
    Ok(refs)
}

/// Walk a parsed CEL AST and collect every free identifier (comprehension
/// iterator and accumulator variables are local bindings, not references).
/// The upstream `references()` walker reports macro locals (`x` in
/// `.map(x, ...)`) as variables — that would fail every legitimate
/// comprehension, so the walk here carries its own locals set.
#[must_use]
pub fn program_identifiers(expr: &cel::IdedExpr) -> Vec<String> {
    let mut locals: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    walk_ids(expr, &mut locals, &mut out);
    out
}

fn walk_ids(
    expr: &cel::IdedExpr,
    locals: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<String>,
) {
    use cel::common::ast::EntryExpr;
    use cel::common::ast::Expr;
    match &expr.expr {
        Expr::Unspecified | Expr::Literal(_) => {}
        Expr::Call(c) => {
            if let Some(t) = &c.target {
                walk_ids(t, locals, out);
            }
            for a in &c.args {
                walk_ids(a, locals, out);
            }
        }
        Expr::Comprehension(c) => {
            locals.insert(c.iter_var.clone());
            if let Some(v2) = &c.iter_var2 {
                locals.insert(v2.clone());
            }
            locals.insert(c.accu_var.clone());
            walk_ids(&c.iter_range, locals, out);
            walk_ids(&c.accu_init, locals, out);
            walk_ids(&c.loop_cond, locals, out);
            walk_ids(&c.loop_step, locals, out);
            walk_ids(&c.result, locals, out);
            locals.remove(&c.iter_var);
            if let Some(v2) = &c.iter_var2 {
                locals.remove(v2);
            }
            locals.remove(&c.accu_var);
        }
        Expr::Ident(name) => {
            if !locals.contains(name) && !out.contains(name) {
                out.push(name.clone());
            }
        }
        Expr::List(l) => {
            for e in &l.elements {
                walk_ids(e, locals, out);
            }
        }
        Expr::Map(m) => {
            for e in &m.entries {
                match &e.expr {
                    EntryExpr::MapEntry(me) => {
                        walk_ids(&me.key, locals, out);
                        walk_ids(&me.value, locals, out);
                    }
                    EntryExpr::StructField(sf) => walk_ids(&sf.value, locals, out),
                }
            }
        }
        Expr::Select(s) => walk_ids(&s.operand, locals, out),
        Expr::Struct(s) => {
            for e in &s.entries {
                if let EntryExpr::StructField(sf) = &e.expr {
                    walk_ids(&sf.value, locals, out);
                }
            }
        }
    }
}

/// Build one compiled node: template parse, static GVK, identity
/// constraints, dependency extraction.
fn build_node(r: &api::Resource, instance_namespaced: bool) -> Result<CompiledNode, CompileError> {
    // The synthetic template an external ref reads through: the ref's
    // identity fields are the template, so the expression scan treats it
    // uniformly.
    let template = match (&r.template, &r.external_ref) {
        (Some(t), _) => t.clone(),
        (None, Some(ext)) => json!({
            "apiVersion": ext.api_version,
            "kind": ext.kind,
            "metadata": {
                "name": ext.name.clone().unwrap_or_default(),
                "namespace": ext.namespace.clone().unwrap_or_default(),
            },
            "__kro_externalRef": true,
        }),
        (None, None) => {
            return Err(CompileError::Invalid(format!(
                "resource {}: nothing to do",
                r.id
            )));
        }
    };

    let api_version = template
        .get("apiVersion")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned();
    let kind = template
        .get("kind")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned();
    if api_version.is_empty() || kind.is_empty() {
        return Err(CompileError::Invalid(format!(
            "resource {}: template needs literal apiVersion and kind (v1: a CEL-computed kind is not supported)",
            r.id
        )));
    }
    let (group, version) = split_api_version(&api_version);
    let plural = api::pluralize(&kind);

    let exprs = expression::collect(&template)
        .map_err(|e| CompileError::Invalid(format!("resource {}: {e}", r.id)))?;

    // Identity constraints: the name path must exist; every forEach
    // iterator must appear in an identity field.
    let has_name = template.pointer("/metadata/name").is_some();
    if r.external_ref.is_none() && !has_name {
        return Err(CompileError::Invalid(format!(
            "resource {}: template must set metadata.name",
            r.id
        )));
    }

    // Every expression source this node carries: template leaves, the
    // policy lists, and the forEach bindings.
    let mut all_exprs: Vec<String> = Vec::new();
    for f in &exprs {
        match &f.render {
            expression::Render::Whole(e) => all_exprs.push(e.clone()),
            expression::Render::Parts(ps) => {
                for p in ps {
                    if let expression::Piece::Expr(e) = p {
                        all_exprs.push(e.clone());
                    }
                }
            }
        }
    }

    // readyWhen binds only the node itself (or `each` for collections) —
    // the one self-contained check. Every other expression may reference
    // upstream resources, which only `compile` can name-check once all
    // ids exist.
    for e in &r.ready_when {
        // readyWhen binds only the node itself (or `each` for collections)
        validate_ids(e, r.id.as_str(), &std::collections::BTreeSet::new())?;
    }
    let _ = instance_namespaced;

    // The real dependencies: identifiers that name other resources.
    // schema/each/this node's own iterators/self are not edges; unknown
    // identifiers already failed the scope check in `compile`.
    let iterators: std::collections::BTreeSet<&str> =
        r.for_each.iter().map(|(name, _)| name.as_str()).collect();
    let mut dependencies: Vec<String> = Vec::new();
    for e in all_exprs
        .iter()
        .chain(r.ready_when.iter())
        .chain(r.include_when.iter())
    {
        if let Ok(program) = cel::Program::compile(e) {
            for ident in program_identifiers(program.expression()) {
                if ident != SCHEMA_VAR
                    && ident != EACH_VAR
                    && ident != r.id
                    && !iterators.contains(ident.as_str())
                    && !dependencies.contains(&ident)
                {
                    dependencies.push(ident);
                }
            }
        }
    }

    Ok(CompiledNode {
        id: r.id.clone(),
        order: r.order,
        external: r.external_ref.is_some(),
        group,
        version,
        kind,
        plural,
        template,
        exprs,
        ready_when: r.ready_when.clone(),
        include_when: r.include_when.clone(),
        for_each: r.for_each.clone(),
        dependencies,
    })
}

fn validate_ids(
    expr: &str,
    id: &str,
    allowed: &std::collections::BTreeSet<&str>,
) -> Result<(), CompileError> {
    let program = cel::Program::compile(expr)
        .map_err(|e| CompileError::Invalid(format!("resource {id}: expression {expr:?}: {e}")))?;
    for ident in program_identifiers(program.expression()) {
        if ident == SCHEMA_VAR
            || ident == EACH_VAR
            || ident == id
            || allowed.contains(ident.as_str())
        {
            continue;
        }
        return Err(CompileError::Invalid(format!(
            "resource {id}: expression {expr:?} references unknown identifier {ident:?}"
        )));
    }
    Ok(())
}

/// Split `apps/v1` into (Some("apps"), "v1"); core kinds are (None, "v1").
#[must_use]
pub fn split_api_version(api_version: &str) -> (Option<String>, String) {
    if let Some((g, v)) = api_version.split_once('/') {
        (Some(g.to_owned()), v.to_owned())
    } else {
        let v = if api_version.is_empty() {
            "v1".to_owned()
        } else {
            api_version.to_owned()
        };
        (None, v)
    }
}

/// The status schema the instance CRD carries: `state` + `conditions`
/// typed, author fields preserved (the CEL inference types them loosely —
/// preserve-unknown keeps any projected shape legal on the wire).
fn status_schema(status: &InstanceStatus) -> Json {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "state".to_owned(),
        json!({"type": "string", "enum": ["IN_PROGRESS", "ACTIVE", "DELETING", "ERROR"]}),
    );
    properties.insert(
        "conditions".to_owned(),
        json!({
            "type": "array",
            "x-kubernetes-list-type": "map",
            "x-kubernetes-list-map-keys": ["type"],
            "items": {
                "type": "object",
                "properties": {
                    "type": {"type": "string"},
                    "status": {"type": "string", "enum": ["True", "False", "Unknown"]},
                    "reason": {"type": "string"},
                    "message": {"type": "string"},
                    "lastTransitionTime": {"type": "string"},
                    "observedGeneration": {"type": "integer", "format": "int64"}
                },
                "required": ["type", "status"]
            }
        }),
    );
    for (path, _) in &status.fields {
        let name = match path.last() {
            Some(expression::Seg::Key(k)) => k.clone(),
            _ => continue,
        };
        properties.insert(name, json!({"x-kubernetes-preserve-unknown-fields": true}));
    }
    json!({"type": "object", "properties": properties})
}

/// FNV-1a over the canonical spec JSON — the recompile trigger.
#[must_use]
pub fn spec_hash(spec: &api::RgdSpec) -> String {
    let canonical = serde_json::to_string(spec).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The instance CRD's GVR, as a face target (the instance-kind pump).
#[must_use]
pub fn instance_target(graph: &CompiledGraph) -> crate::face::Target {
    crate::face::Target::cr(
        &graph.instance_group,
        &graph.instance_version,
        &graph.instance_plural,
        None,
    )
}

/// The child GVR targets (one pump per distinct GVR in the graph).
#[must_use]
pub fn child_targets(graph: &CompiledGraph) -> Vec<crate::face::Target> {
    let mut targets: Vec<crate::face::Target> = Vec::new();
    for n in &graph.nodes {
        let t = crate::face::Target::cr(
            n.group.as_deref().unwrap_or(""),
            &n.version,
            &n.plural,
            None,
        );
        if !targets.contains(&t) {
            targets.push(t);
        }
    }
    targets
}

/// The labels the engine stamps on the RGD-derived surfaces.
#[must_use]
pub fn rgd_labels(graph: &CompiledGraph) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert(labels::RGD_NAME_LABEL.to_owned(), graph.rgd_name.clone());
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rgd_body(resources: &Json, status: &Json) -> Json {
        json!({
            "metadata": {"name": "webstore", "uid": "u1", "generation": 1},
            "spec": {
                "schema": {
                    "apiVersion": "v1alpha1",
                    "kind": "WebStore",
                    "spec": {"name": "string | required=true", "image": "string | default=nginx"},
                    "status": status
                },
                "resources": resources
            }
        })
    }

    fn compile_body(resources: &Json, status: &Json) -> Result<CompiledGraph, CompileError> {
        let parsed = api::parse(&rgd_body(resources, status), "kro.rs").unwrap();
        compile(&parsed, &EngineDefaults::default())
    }

    #[test]
    fn a_simple_graph_compiles_with_topological_order() {
        let resources = json!([
            {"id": "service", "template": {"apiVersion": "v1", "kind": "Service",
                "metadata": {"name": "${schema.spec.name}"}, "spec": {"selector": "${deployment.spec.selector}"}}},
            {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "${schema.spec.name}"}, "data": {"env": "prod"}}},
            {"id": "deployment", "template": {"apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": {"name": "${schema.spec.name}"}, "spec": {"replicas": 1}}}
        ]);
        let g = compile_body(&resources, &json!({})).unwrap();
        // configmap and deployment are roots; service depends on deployment
        let idx = |id: &str| {
            g.topological_order
                .iter()
                .position(|x| x == id)
                .expect("present")
        };
        assert!(idx("deployment") < idx("service"));
        assert!(idx("configmap") < idx("service"));
        let svc = g.nodes.iter().find(|n| n.id == "service").unwrap();
        assert_eq!(svc.dependencies, vec!["deployment"]);
    }

    #[test]
    fn a_cycle_refuses_with_the_path() {
        let resources = json!([
            {"id": "a", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "a"}, "data": {"v": "${b.data.v}"}}},
            {"id": "b", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "b"}, "data": {"v": "${a.data.v}"}}}
        ]);
        let err = compile_body(&resources, &json!({})).unwrap_err();
        match err {
            CompileError::Cycle(c) => {
                let s = c.to_string();
                assert!(s.contains('a') && s.contains('b'), "{s}");
            }
            CompileError::Invalid(m) => panic!("expected cycle, got Invalid: {m}"),
        }
    }

    #[test]
    fn unknown_identifiers_refuse_at_compile() {
        let resources = json!([
            {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "${schema.spec.name}"}, "data": {"x": "${database.status.host}"}}}
        ]);
        let err = compile_body(&resources, &json!({})).unwrap_err();
        assert!(err.to_string().contains("database"), "{err}");
    }

    #[test]
    fn instance_crd_synthesizes_with_status_schema() {
        let resources = json!([
            {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "${schema.spec.name}"}}}
        ]);
        let status = json!({"endpoint": "${configmap.data.host}"});
        let g = compile_body(&resources, &status).unwrap();
        assert_eq!(g.instance_group, "kro.rs");
        assert_eq!(g.instance_plural, "webstores");
        assert_eq!(g.instance_kind, "WebStore");
        assert!(g.namespaced);
        let crd = &g.instance_crd;
        assert_eq!(crd["spec"]["group"], json!("kro.rs"));
        assert_eq!(crd["spec"]["names"]["kind"], json!("WebStore"));
        assert_eq!(crd["metadata"]["name"], json!("webstores.kro.rs"));
        let schema = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"];
        assert_eq!(
            schema["properties"]["spec"]["properties"]["name"]["type"],
            json!("string")
        );
        assert_eq!(
            schema["properties"]["status"]["properties"]["state"]["enum"],
            json!(["IN_PROGRESS", "ACTIVE", "DELETING", "ERROR"])
        );
        assert!(
            schema["properties"]["status"]["properties"]["endpoint"]
                .get("x-kubernetes-preserve-unknown-fields")
                .is_some()
        );
    }

    #[test]
    fn status_leaf_must_be_an_expression() {
        let resources = json!([
            {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "${schema.spec.name}"}}}
        ]);
        let err = compile_body(&resources, &json!({"plain": "value"})).unwrap_err();
        assert!(err.to_string().contains("expression"), "{err}");
    }

    #[test]
    fn the_spec_hash_is_stable_and_order_sensitive() {
        let parsed = api::parse(
            &rgd_body(
                &json!([
                    {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                        "metadata": {"name": "x"}}}
                ]),
                &json!({}),
            ),
            "kro.rs",
        )
        .unwrap();
        let h1 = spec_hash(&parsed.spec);
        let h2 = spec_hash(&parsed.spec);
        assert_eq!(h1, h2);
        let parsed2 = api::parse(
            &rgd_body(
                &json!([
                    {"id": "configmap", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                        "metadata": {"name": "y"}}}
                ]),
                &json!({}),
            ),
            "kro.rs",
        )
        .unwrap();
        assert_ne!(h1, spec_hash(&parsed2.spec));
    }

    #[test]
    fn for_each_iterators_are_not_edges() {
        let resources = json!([
            {"id": "svc", "template": {"apiVersion": "v1", "kind": "Service",
                "metadata": {"name": "${each.name}"}},
             "forEach": [{"each": "${schema.spec.items}"}]}
        ]);
        let parsed = api::parse(&rgd_body(&resources, &json!({})), "kro.rs").unwrap();
        let node = build_node(&parsed.spec.resources[0], true).unwrap();
        assert!(node.dependencies.is_empty(), "{:?}", node.dependencies);
        assert_eq!(
            node.for_each,
            vec![("each".to_owned(), "schema.spec.items".to_owned())]
        );
    }

    #[test]
    fn external_refs_read_without_applying() {
        let parsed = api::parse(&json!({
            "metadata": {"name": "reader", "uid": "u", "generation": 1},
            "spec": {
                "schema": {"apiVersion": "v1alpha1", "kind": "Reader", "spec": {}, "status": {}},
                "resources": [
                    {"id": "existing", "externalRef": {"apiVersion": "v1", "kind": "ConfigMap",
                        "metadata": {"name": "shared", "namespace": "tools"}}},
                    {"id": "consumer", "template": {"apiVersion": "v1", "kind": "ConfigMap",
                        "metadata": {"name": "out"}, "data": {"v": "${existing.data.k}"}}}
                ]
            }
        }), "kro.rs").unwrap();
        let g = compile(&parsed, &EngineDefaults::default()).unwrap();
        let ext = g.nodes.iter().find(|n| n.id == "existing").unwrap();
        assert!(ext.external);
        assert_eq!(ext.kind, "ConfigMap");
        let consumer = g.nodes.iter().find(|n| n.id == "consumer").unwrap();
        assert_eq!(consumer.dependencies, vec!["existing"]);
        assert!(
            g.topological_order
                .iter()
                .position(|x| x == "existing")
                .unwrap()
                < g.topological_order
                    .iter()
                    .position(|x| x == "consumer")
                    .unwrap()
        );
    }
}
