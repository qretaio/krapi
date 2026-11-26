//! THE STANDALONE E2E — the kro engine, running as its own process
//! would, rides kube-rs against a REAL kapi server (the axum HTTP face
//! over the sqlite store): the RGD CRD self-bootstraps, the RGD
//! compiles, the instance CRD registers at runtime, an instance
//! reconciles its children in dependency order, readiness propagates,
//! and the status projects back onto the instance. This is the embed
//! proof too — the same face shape the kapi fabric lane serves.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use krapi::face::{SharedFace, Target};
use krapi::{Engine, EngineConfig};
use std::sync::Arc;
use std::time::Duration;

async fn server() -> String {
    let dir = tempfile::tempdir().unwrap();
    let conn = Arc::new(
        kapi_server::store::open_pool(dir.path().join("e2e.db").to_str().unwrap(), 2).unwrap(),
    );
    let router = kapi_server::api::router(Arc::new(kapi_server::api::AppState::new(
        Arc::clone(&conn),
        None,
    )));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    #[allow(clippy::mem_forget)]
    std::mem::forget(dir);
    format!("http://{addr}")
}

/// The RGD under test: one child CRD whose spec mirrors the instance
/// spec, gated on a second child's status (the classic two-step).
const RGD: &str = r#"{
    "apiVersion": "kro.run/v1alpha1",
    "kind": "ResourceGraphDefinition",
    "metadata": {"name": "webstore"},
    "spec": {
        "schema": {
            "apiVersion": "v1alpha1",
            "kind": "WebStore",
            "spec": {"name": "string | required=true", "image": "string | default=nginx"},
            "status": {"imageOut": "${frontend.spec.image}"}
        },
        "resources": [
            {"id": "frontend", "template": {
                "apiVersion": "test.io/v1", "kind": "Widget",
                "metadata": {"name": "${schema.spec.name}--front"},
                "spec": {"image": "${schema.spec.image}", "ready": true}
            }},
            {"id": "backend", "template": {
                "apiVersion": "test.io/v1", "kind": "Widget",
                "metadata": {"name": "${schema.spec.name}--back"},
                "spec": {"image": "${frontend.spec.image}", "ready": true}
            }}
        ]
    }
}"#;

async fn http_json(
    method: &str,
    url: &str,
    path: &str,
    body: Option<String>,
) -> (u16, serde_json::Value) {
    let client = reqwest::Client::new();
    let mut req = client.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
        format!("{url}{path}"),
    );
    if let Some(b) = body {
        req = req.header("content-type", "application/json").body(b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

async fn wait_for<F, Fut, T>(secs: u64, what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        assert!(
            tokio::time::Instant::now() <= deadline,
            "timed out waiting for {what}"
        );
        if let Some(t) = f().await {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // the scenario reads best as one linear story
async fn the_standalone_engine_reconciles_against_a_kapi_server() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info,kro=debug"))
        .with_test_writer()
        .try_init();
    let url = server().await;

    // The engine rides kube-rs — the standalone face, exactly as the
    // runner binary drives it.
    let client = krapi_kube::connect(&url, None, "default").unwrap();
    let face = SharedFace::erase(krapi_kube::KubeFace::new(client));
    krapi_kube::ensure_rgd_crd(&face, "kro.run", "v1alpha1")
        .await
        .expect("the RGD CRD installs");

    let config = EngineConfig {
        workers: 2,
        ..EngineConfig::default()
    };
    tokio::spawn(async move { Engine::new(face, config).run().await });

    // A child kind the RGD's templates target — any user CRD.
    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/apiextensions.k8s.io/v1/customresourcedefinitions",
        Some(
            r#"{
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "widgets.test.io"},
            "spec": {
                "group": "test.io", "scope": "Namespaced",
                "names": {"plural": "widgets", "singular": "widget", "kind": "Widget", "listKind": "WidgetList"},
                "versions": [{"name": "v1", "served": true, "storage": true,
                    "subresources": {"status": {}},
                    "schema": {"openAPIV3Schema": {"type": "object", "properties": {
                        "spec": {"type": "object", "x-kubernetes-preserve-unknown-fields": true}}}}}]
            }
        }"#
            .to_owned(),
        ),
    )
    .await;
    assert_eq!(code, 201, "the child CRD registers");

    // The RGD compiles (its acceptance shows in the status conditions).
    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/kro.run/v1alpha1/resourcegraphdefinitions",
        Some(RGD.to_owned()),
    )
    .await;
    assert_eq!(code, 201, "the RGD applies");

    let mut last: Option<serde_json::Value> = None;
    let accepted = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let (code, body) = http_json(
                "GET",
                &url,
                "/apis/kro.run/v1alpha1/resourcegraphdefinitions/webstore",
                None,
            )
            .await;
            if code == 200 {
                last = Some(body.clone());
                let accepted = body
                    .pointer("/status/conditions")
                    .and_then(|c| c.as_array())
                    .is_some_and(|cs| {
                        cs.iter()
                            .any(|c| c["type"] == "GraphAccepted" && c["status"] == "True")
                    });
                if accepted {
                    break body;
                }
            }
            assert!(
                tokio::time::Instant::now() <= deadline,
                "timed out waiting for the RGD accepted; last body: {last:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    let _ = accepted;

    // The instance CRD registered at runtime.
    wait_for(20, "the instance CRD", || async {
        let (code, body) = http_json(
            "GET",
            &url,
            "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/webstores.kro.run",
            None,
        )
        .await;
        (code == 200).then_some(body)
    })
    .await;

    // An instance reconciles: children in order, status projected.
    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/kro.run/v1alpha1/namespaces/default/webstores",
        Some(
            r#"{"apiVersion": "kro.run/v1alpha1", "kind": "WebStore",
            "metadata": {"name": "shop"},
            "spec": {"name": "shop", "image": "nginx:1.25"}}"#
                .to_owned(),
        ),
    )
    .await;
    assert_eq!(code, 201, "the instance applies");

    let frontend = wait_for(30, "the frontend widget", || async {
        let (code, body) = http_json(
            "GET",
            &url,
            "/apis/test.io/v1/namespaces/default/widgets/shop--front",
            None,
        )
        .await;
        (code == 200).then_some(body)
    })
    .await;
    assert_eq!(
        frontend.pointer("/spec/image"),
        Some(&serde_json::json!("nginx:1.25")),
        "the instance spec interpolated into the child"
    );

    let backend = wait_for(30, "the backend widget", || async {
        let (code, body) = http_json(
            "GET",
            &url,
            "/apis/test.io/v1/namespaces/default/widgets/shop--back",
            None,
        )
        .await;
        (code == 200).then_some(body)
    })
    .await;
    assert_eq!(
        backend.pointer("/spec/image"),
        Some(&serde_json::json!("nginx:1.25")),
        "the child's spec flowed to its dependent"
    );

    // Status projection: ACTIVE with the author field, plus the built-in
    // conditions and the finalizer.
    let inst = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut last = None;
        loop {
            let (code, body) = http_json(
                "GET",
                &url,
                "/apis/kro.run/v1alpha1/namespaces/default/webstores/shop",
                None,
            )
            .await;
            if code == 200 {
                last = Some(body.clone());
                if body
                    .pointer("/status/state")
                    .and_then(serde_json::Value::as_str)
                    == Some("ACTIVE")
                {
                    break body;
                }
            }
            assert!(
                tokio::time::Instant::now() <= deadline,
                "timed out waiting for the instance ACTIVE; last: {last:?}"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    assert_eq!(
        inst.pointer("/status/imageOut"),
        Some(&serde_json::json!("nginx:1.25"))
    );
    let conds = inst
        .pointer("/status/conditions")
        .and_then(|c| c.as_array())
        .expect("conditions");
    assert!(
        conds
            .iter()
            .any(|c| c["type"] == "Ready" && c["status"] == "True"),
        "the root folds: {conds:?}"
    );
    assert!(
        inst.pointer("/metadata/finalizers")
            .and_then(|f| f.as_array())
            .is_some_and(|f| f.iter().any(|x| x.as_str() == Some("kro.run/finalizer")))
    );

    // Deletion: children drain, the finalizer drops, the row goes.
    let (code, _) = http_json(
        "DELETE",
        &url,
        "/apis/kro.run/v1alpha1/namespaces/default/webstores/shop",
        None,
    )
    .await;
    assert_eq!(code, 200, "the delete stamps (finalizer holds)");

    wait_for(30, "the children pruned", || async {
        let (code, _) = http_json(
            "GET",
            &url,
            "/apis/test.io/v1/namespaces/default/widgets/shop--front",
            None,
        )
        .await;
        (code == 404).then_some(())
    })
    .await;

    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let (code, body) = http_json(
                "GET",
                &url,
                "/apis/kro.run/v1alpha1/namespaces/default/webstores/shop",
                None,
            )
            .await;
            if code == 404 {
                break;
            }
            assert!(
                tokio::time::Instant::now() <= deadline,
                "timed out waiting for the instance drained; last: {body}"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    let _ = Target::crds(); // the vocabulary stays exercised
}
