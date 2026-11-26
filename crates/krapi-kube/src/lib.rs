//! THE STANDALONE FACE — kro's [`Face`] over kube-rs: the same
//! list/watch/apply grammar the engine rides, carried over HTTP to any
//! Kubernetes-shaped API server (a real cluster or a kapi server).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use futures_util::StreamExt;
use krapi::face::{Action, Face, FaceError, Listed, Target, WatchEvent};
use kube::api::{
    Api, ApiResource, DynamicObject, ListParams, Patch, PatchParams, PostParams, WatchParams,
};
use std::future::Future;
use tokio::sync::mpsc;

/// A plain-URL client config (bearer token, no kubeconfig ceremony).
///
/// # Errors
/// A bad URL or client construction failure.
pub fn connect(
    url: &str,
    token: Option<&str>,
    default_namespace: &str,
) -> anyhow::Result<KubeClient> {
    let mut cfg = kube::Config::new(url.parse()?);
    if let Some(token) = token {
        cfg.auth_info.token = Some(secrecy::SecretString::from(token.to_owned()));
    }
    default_namespace.clone_into(&mut cfg.default_namespace);
    Ok(KubeClient::try_from(cfg)?)
}

/// The kube-backed face: one client, every surface.
#[derive(Clone)]
pub struct KubeFace {
    client: KubeClient,
}

// the kube client carries no Debug of its own — the shape is what a log line needs
impl std::fmt::Debug for KubeFace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KubeFace(..)")
    }
}

impl KubeFace {
    /// Wrap a kube-rs client.
    #[must_use]
    pub fn new(client: KubeClient) -> Self {
        Self { client }
    }

    fn api(&self, target: &Target) -> Api<DynamicObject> {
        let ar = api_resource(target);
        match &target.namespace {
            Some(ns) => Api::namespaced_with(self.client.clone(), ns, &ar),
            None => Api::all_with(self.client.clone(), &ar),
        }
    }

    fn named(&self, target: &Target, namespace: Option<&str>) -> Api<DynamicObject> {
        let ar = api_resource(target);
        match namespace.or(target.namespace.as_deref()) {
            Some(ns) => Api::namespaced_with(self.client.clone(), ns, &ar),
            None => Api::all_with(self.client.clone(), &ar),
        }
    }
}

/// The `ApiResource` for a target (the gvk + plural the dynamic client needs).
fn api_resource(target: &Target) -> ApiResource {
    let group = target.group.clone().unwrap_or_default();
    let kind = plural_kind(target);
    ApiResource::from_gvk_with_plural(
        &kube::core::GroupVersionKind::gvk(&group, &target.version, &kind),
        &target.plural,
    )
}

/// The kind name for a plural (the singular with the CRD convention —
/// good enough for the dynamic client's URL, which never uses the kind).
fn plural_kind(target: &Target) -> String {
    let mut kind = target.plural.trim_end_matches('s').to_owned();
    if let Some(first) = kind.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    kind
}

fn face_error(e: &kube::Error) -> FaceError {
    if let kube::Error::Api(ae) = e {
        if ae.code == 410 {
            return FaceError::Gone;
        }
        if ae.code == 404 {
            return FaceError::NotFound(ae.message.clone());
        }
        if ae.code == 422 || ae.code == 400 {
            return FaceError::Invalid(ae.message.clone());
        }
    }
    FaceError::Other(e.to_string())
}

impl Face for KubeFace {
    fn list(&self, target: &Target) -> impl Future<Output = Result<Listed, FaceError>> + Send {
        let api = self.api(target);
        let params = ListParams::default();
        async move {
            let list = api.list(&params).await.map_err(|e| face_error(&e))?;
            let rv = list.metadata.resource_version.clone();
            let items = list
                .into_iter()
                .map(|o| {
                    serde_json::to_value(o)
                        .map_err(|e| FaceError::Other(format!("object encode: {e}")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Listed {
                items,
                resource_version: rv,
            })
        }
    }

    fn watch(
        &self,
        target: &Target,
        resource_version: Option<&str>,
    ) -> impl Future<Output = Result<mpsc::Receiver<WatchEvent>, FaceError>> + Send {
        let api = self.api(target);
        let rv = resource_version.unwrap_or("0").to_owned();
        async move {
            let wp = WatchParams {
                bookmarks: true,
                ..Default::default()
            };
            let stream = api.watch(&wp, &rv).await.map_err(|e| face_error(&e))?;
            let (tx, rx) = mpsc::channel::<WatchEvent>(64);
            // THE PUMP: the wire's stream into the engine's channel
            tokio::spawn(async move {
                let mut stream = std::pin::pin!(stream);
                while let Some(item) = stream.next().await {
                    let event = match item {
                        Ok(kube::api::WatchEvent::Added(o)) => {
                            serde_json::to_value(o).ok().map(WatchEvent::Added)
                        }
                        Ok(kube::api::WatchEvent::Modified(o)) => {
                            serde_json::to_value(o).ok().map(WatchEvent::Modified)
                        }
                        Ok(kube::api::WatchEvent::Deleted(o)) => {
                            serde_json::to_value(o).ok().map(WatchEvent::Deleted)
                        }
                        Ok(kube::api::WatchEvent::Bookmark(b)) => {
                            Some(WatchEvent::Bookmark(kube_core::watch::Bookmark {
                                types: kube_core::metadata::TypeMeta {
                                    api_version: b.types.api_version,
                                    kind: b.types.kind,
                                },
                                metadata: kube_core::watch::BookmarkMeta {
                                    resource_version: b.metadata.resource_version,
                                    annotations: b.metadata.annotations,
                                },
                            }))
                        }
                        Ok(kube::api::WatchEvent::Error(status)) => {
                            // an in-stream error ends the pump — the
                            // reconnect rides the 410 heal
                            tracing::warn!(
                                code = status.code,
                                "kro-kube: watch stream errored — reconnecting"
                            );
                            break;
                        }
                        Err(e) => {
                            tracing::warn!("kro-kube: watch transport failed: {e} — reconnecting");
                            break;
                        }
                    };
                    let Some(event) = event else { continue };
                    if tx.send(event).await.is_err() {
                        break; // the engine is gone — the pump follows
                    }
                }
            });
            Ok(rx)
        }
    }

    fn apply(&self, action: &Action) -> impl Future<Output = Result<(), FaceError>> + Send {
        let this = self.clone();
        let action = action.clone();
        async move {
            match action {
                Action::Create { target, body } => {
                    let api = this.named(&target, None);
                    let obj: DynamicObject = serde_json::from_value(body)
                        .map_err(|e| FaceError::Invalid(format!("body decode: {e}")))?;
                    api.create(&PostParams::default(), &obj)
                        .await
                        .map_err(|e| face_error(&e))?;
                    Ok(())
                }
                Action::Patch {
                    target,
                    namespace,
                    name,
                    body,
                } => {
                    let api = this.named(&target, namespace.as_deref());
                    api.patch(&name, &PatchParams::default(), &Patch::Merge(body))
                        .await
                        .map_err(|e| face_error(&e))?;
                    Ok(())
                }
                Action::PatchStatus {
                    target,
                    namespace,
                    name,
                    body,
                } => {
                    let api = this.named(&target, namespace.as_deref());
                    api.patch_subresource(
                        "status",
                        &name,
                        &PatchParams::default(),
                        &Patch::Merge(body),
                    )
                    .await
                    .map_err(|e| face_error(&e))?;
                    Ok(())
                }
                Action::Delete {
                    target,
                    namespace,
                    name,
                } => {
                    let api = this.named(&target, namespace.as_deref());
                    api.delete(&name, &kube::api::DeleteParams::default())
                        .await
                        .map_err(|e| face_error(&e))?;
                    Ok(())
                }
            }
        }
    }
}

pub use kube::Client as KubeClient;

/// Create the RGD surface when the server does not serve it yet
/// (create-if-absent; `AlreadyExists` means someone beat us to it).
/// # Errors
/// The face refused the create after the retries.
pub async fn ensure_rgd_crd(face: &impl Face, group: &str, version: &str) -> Result<(), FaceError> {
    let crd = serde_json::json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": format!("resourcegraphdefinitions.{group}")},
        "spec": {
            "group": group,
            "scope": "Cluster",
            "names": {
                "plural": "resourcegraphdefinitions",
                "singular": "resourcegraphdefinition",
                "kind": "ResourceGraphDefinition",
                "listKind": "ResourceGraphDefinitionList",
                "shortNames": ["rgd"]
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
                            "spec": {
                                "type": "object",
                                "x-kubernetes-preserve-unknown-fields": true
                            },
                            "status": {
                                "type": "object",
                                "x-kubernetes-preserve-unknown-fields": true
                            }
                        }
                    }
                }
            }]
        }
    });
    for attempt in 1..=30u32 {
        match Face::apply(
            face,
            &Action::Create {
                target: Target::crds(),
                body: crd.clone(),
            },
        )
        .await
        {
            Ok(()) => {
                tracing::info!(group = %group, "kro: the RGD CRD is installed");
                return Ok(());
            }
            Err(FaceError::Invalid(m) | FaceError::Other(m))
                if m.contains("already exists") || m.contains("AlreadyExists") =>
            {
                tracing::info!(group = %group, "kro: the RGD CRD is already served");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(attempt, "kro: the RGD CRD install failed ({e}) — retrying");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if attempt == 30 {
                    return Err(e);
                }
            }
        }
    }
    Err(FaceError::Other(
        "the RGD CRD install never settled".to_owned(),
    ))
}
