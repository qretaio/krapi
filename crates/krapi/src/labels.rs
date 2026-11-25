//! THE IDENTITY MARKS — every child the engine applies carries the same
//! label family, and every read (ownership resolution, orphan pruning,
//! deletion waves) is driven by them. The prefix is `kro.rs` (this
//! project's domain), the conventions upstream kro's.

/// The label/annotation prefix.
pub const LABEL_PREFIX: &str = "kro.rs";

/// The instance finalizer: an instance with this finalizer survives its
/// DELETE until the children drain.
pub const FINALIZER: &str = "kro.rs/finalizer";

/// `app.kubernetes.io/managed-by` value.
pub const MANAGED_BY_VALUE: &str = "kro-rs";
/// The managed-by key.
pub const MANAGED_BY_KEY: &str = "app.kubernetes.io/managed-by";

/// The node id that produced the child (forEach expansions share it).
pub const NODE_ID_LABEL: &str = "kro.rs/node-id";
/// The child is engine-owned.
pub const OWNED_LABEL: &str = "kro.rs/owned";
/// The reverse-topological deletion wave the child sits in.
pub const APPLY_ORDER_LABEL: &str = "kro.rs/apply-order";

/// The owning instance's identity labels (the child → instance join).
pub const INSTANCE_NAME_LABEL: &str = "kro.rs/instance-name";
pub const INSTANCE_NAMESPACE_LABEL: &str = "kro.rs/instance-namespace";
pub const INSTANCE_UID_LABEL: &str = "kro.rs/instance-uid";
pub const INSTANCE_GROUP_LABEL: &str = "kro.rs/instance-group";
pub const INSTANCE_VERSION_LABEL: &str = "kro.rs/instance-version";
pub const INSTANCE_KIND_LABEL: &str = "kro.rs/instance-kind";

/// The RGD that compiled the graph the child came from.
pub const RGD_NAME_LABEL: &str = "kro.rs/rgd-name";

/// Stamp the instance-identity labels onto a child object. The instance
/// labels are the join key: a child event resolves its owning instance
/// from these, which is what drives readiness propagation without any
/// per-instance watch bookkeeping.
pub fn stamp_instance_labels(
    obj: &mut serde_json::Value,
    instance: &serde_json::Value,
    kind_gv: (&str, &str),
) {
    let meta = obj.as_object_mut().and_then(|o| {
        o.entry("metadata")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
    });
    let Some(meta) = meta else {
        return;
    };
    let mut labels = meta
        .get("labels")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (k, v) in [
        (INSTANCE_NAME_LABEL, "/metadata/name"),
        (INSTANCE_NAMESPACE_LABEL, "/metadata/namespace"),
        (INSTANCE_UID_LABEL, "/metadata/uid"),
        (INSTANCE_KIND_LABEL, "/kind"),
        (INSTANCE_GROUP_LABEL, "/apiVersion-group"),
        (INSTANCE_VERSION_LABEL, "/apiVersion-version"),
    ] {
        if let Some(v) = instance.pointer(v).and_then(serde_json::Value::as_str) {
            labels.insert(k.to_owned(), serde_json::Value::String(v.to_owned()));
        }
    }
    meta.insert("labels".to_owned(), serde_json::Value::Object(labels));
    let _ = kind_gv;
}

/// The owning instance key carried by a child's labels, if the child is
/// engine-owned (`(namespace, name)`).
#[must_use]
pub fn owning_instance(obj: &serde_json::Value) -> Option<(Option<String>, String)> {
    let owned = obj
        .pointer("/metadata/labels/kro.rs~1owned")
        .and_then(serde_json::Value::as_str)
        == Some("true");
    let name = obj
        .pointer(&format!("/metadata/labels/{INSTANCE_NAME_LABEL}"))
        .and_then(serde_json::Value::as_str)?;
    let ns = obj
        .pointer(&format!("/metadata/labels/{INSTANCE_NAMESPACE_LABEL}"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    if owned {
        Some((ns, name.to_owned()))
    } else {
        None
    }
}
