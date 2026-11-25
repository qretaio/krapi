//! THE FACE — the engine's entire I/O surface. Three verbs: list, watch,
//! apply. The engine holds no cluster knowledge of its own: every read is a
//! list (the pump) and every write is an [`Action`], so the same laws run
//! over HTTP (standalone) or an inproc lane (embedded).
//!
//! The vocabulary mirrors the kapi controller's one-to-one
//! ([`Target`]/[`Action`]/`WatchEvent`), which is what makes the embed
//! adapter a thin rewrite rather than a translation.

use serde_json::Value;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;

/// One watch surface: a group (None = core-v1), a version, a plural, and an
/// optional namespace (None = cluster-wide).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Target {
    pub group: Option<String>,
    pub version: String,
    pub plural: String,
    pub namespace: Option<String>,
}

impl Target {
    /// A CR-surface target. An empty group normalizes to core-v1 (the
    /// template `apiVersion: v1` shape), so cache keys never diverge
    /// between the engine and the faces.
    #[must_use]
    pub fn cr(group: &str, version: &str, plural: &str, namespace: Option<&str>) -> Self {
        Self {
            group: (!group.is_empty()).then(|| group.to_owned()),
            version: version.to_owned(),
            plural: plural.to_owned(),
            namespace: namespace.map(str::to_owned),
        }
    }

    /// The apiextensions target — the CRD surface itself (instance kinds
    /// register here at runtime).
    #[must_use]
    pub fn crds() -> Self {
        Self::cr(
            "apiextensions.k8s.io",
            "v1",
            "customresourcedefinitions",
            None,
        )
    }

    /// The stable identity used for loop bookkeeping and log lines.
    #[must_use]
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}@{}",
            self.group.as_deref().unwrap_or("core"),
            self.version,
            self.plural,
            self.namespace.as_deref().unwrap_or("*")
        )
    }

    /// The list/watch URL path (the generic k8s grammar both faces speak).
    #[must_use]
    pub fn path(&self) -> String {
        let head = match &self.group {
            Some(g) => format!("/apis/{g}/{}", self.version),
            None => format!("/api/{}", self.version),
        };
        match &self.namespace {
            Some(ns) => format!("{head}/namespaces/{ns}/{}", self.plural),
            None => format!("{head}/{}", self.plural),
        }
    }
}

/// A list's answer: the rows AND the snapshot they came from — the watch
/// must start from this version (the list→watch gap law).
#[derive(Debug)]
pub struct Listed {
    pub items: Vec<Value>,
    pub resource_version: Option<String>,
}

/// One verb, as data: the engine's writes are carried to the cluster.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Action {
    Create {
        target: Target,
        body: Value,
    },
    /// The merge the surface serves: new keys win, arrays replace.
    Patch {
        target: Target,
        namespace: Option<String>,
        name: String,
        body: Value,
    },
    /// The status subresource (the report-back face).
    PatchStatus {
        target: Target,
        namespace: Option<String>,
        name: String,
        body: Value,
    },
    Delete {
        target: Target,
        namespace: Option<String>,
        name: String,
    },
}

impl Action {
    /// A short human label for the logs.
    #[must_use]
    pub fn verb(&self) -> &'static str {
        match self {
            Action::Create { .. } => "create",
            Action::Patch { .. } => "patch",
            Action::PatchStatus { .. } => "patch-status",
            Action::Delete { .. } => "delete",
        }
    }

    /// The target the action rides.
    #[must_use]
    pub fn target(&self) -> &Target {
        match self {
            Action::Create { target, .. }
            | Action::Patch { target, .. }
            | Action::PatchStatus { target, .. }
            | Action::Delete { target, .. } => target,
        }
    }
}

/// One watch event — the upstream envelope
/// ([`kube_core::watch::WatchEvent`]) over the `Value` currency, REUSED
/// not copied: kapi's controller crate and this engine share the type
/// (and in an embedded deployment, the binary), so the adapter is a
/// zero-rewrite pass-through and every consumer sees one shape.
pub type WatchEvent = kube_core::watch::WatchEvent<Value>;

/// Why a face call failed. [`FaceError::Gone`] is the 410 — the version
/// fell off the feed; the loop heals by re-list. [`FaceError::NotFound`]
/// means the surface itself is not there yet (an instance CRD still
/// establishing).
#[derive(Debug, Clone, thiserror::Error)]
pub enum FaceError {
    #[error("the resource version is gone (410) — re-list")]
    Gone,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("refused: {0}")]
    Invalid(String),
    #[error("{0}")]
    Other(String),
}

/// The face: the connection half. The engine drives it; it never decides.
pub trait Face: Send + Sync + 'static {
    /// The full list of one target, carrying the snapshot version the
    /// subsequent watch must start from.
    fn list(&self, target: &Target) -> impl Future<Output = Result<Listed, FaceError>> + Send;

    /// Watch one target from a resource version, streaming events into the
    /// returned channel (the face owns the pump task).
    fn watch(
        &self,
        target: &Target,
        resource_version: Option<&str>,
    ) -> impl Future<Output = Result<mpsc::Receiver<WatchEvent>, FaceError>> + Send;

    /// Carry one action to the cluster.
    fn apply(&self, action: &Action) -> impl Future<Output = Result<(), FaceError>> + Send;
}

type BoxFuture<T> = std::pin::Pin<Box<dyn Future<Output = T> + Send>>;

/// Any face, erased — the engine holds one and hands clones to every pump.
/// The [`Face`] trait is not object-safe (RPITIT), so the erasure mirrors
/// the kapi controller's `SharedExecutor` shape.
fn list_fn<F: Face>(face: Arc<F>) -> ListFn {
    Arc::new(move |target: Target| {
        let face = Arc::clone(&face);
        Box::pin(async move { face.list(&target).await })
    })
}

fn watch_fn<F: Face>(face: Arc<F>) -> WatchFn {
    Arc::new(move |target: Target, rv: Option<String>| {
        let face = Arc::clone(&face);
        Box::pin(async move { face.watch(&target, rv.as_deref()).await })
    })
}

fn apply_fn<F: Face>(face: Arc<F>) -> ApplyFn {
    Arc::new(move |action: Action| {
        let face = Arc::clone(&face);
        Box::pin(async move { face.apply(&action).await })
    })
}

type ListFn = Arc<dyn Fn(Target) -> BoxFuture<Result<Listed, FaceError>> + Send + Sync>;
type WatchFn = Arc<
    dyn Fn(Target, Option<String>) -> BoxFuture<Result<mpsc::Receiver<WatchEvent>, FaceError>>
        + Send
        + Sync,
>;
type ApplyFn = Arc<dyn Fn(Action) -> BoxFuture<Result<(), FaceError>> + Send + Sync>;

#[derive(Clone)]
pub struct SharedFace {
    list: ListFn,
    watch: WatchFn,
    apply: ApplyFn,
}

impl std::fmt::Debug for SharedFace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedFace").finish_non_exhaustive()
    }
}

impl SharedFace {
    /// Erase any face into the shared, cloneable form.
    #[must_use]
    pub fn erase<F: Face>(face: F) -> Self {
        let face = Arc::new(face);
        Self {
            list: list_fn(Arc::clone(&face)),
            watch: watch_fn(Arc::clone(&face)),
            apply: apply_fn(face),
        }
    }
}

impl Face for SharedFace {
    fn list(&self, target: &Target) -> impl Future<Output = Result<Listed, FaceError>> + Send {
        (self.list)(target.clone())
    }

    fn watch(
        &self,
        target: &Target,
        resource_version: Option<&str>,
    ) -> impl Future<Output = Result<mpsc::Receiver<WatchEvent>, FaceError>> + Send {
        (self.watch)(target.clone(), resource_version.map(str::to_owned))
    }

    fn apply(&self, action: &Action) -> impl Future<Output = Result<(), FaceError>> + Send {
        (self.apply)(action.clone())
    }
}
