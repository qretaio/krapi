//! THE DELETED-DELIVERY PROBE — a watch on a CR surface must answer
//! creates AND deletes (the tombstone). The E2E's deletion drain hangs
//! on exactly this: the child cache never learns a row died.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use krapi::face::Face;
use std::sync::Arc;
use std::time::Duration;

async fn server() -> String {
    let dir = tempfile::tempdir().unwrap();
    let conn = Arc::new(
        kapi_server::store::open_pool(dir.path().join("probe.db").to_str().unwrap(), 2).unwrap(),
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

#[tokio::test(flavor = "multi_thread")]
async fn the_watch_answers_the_delete() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info,kro_kube=debug"))
        .try_init();
    let url = server().await;

    let client = krapi_kube::connect(&url, None, "default").unwrap();
    let face = krapi::face::SharedFace::erase(krapi_kube::KubeFace::new(client));

    let target = krapi::face::Target::cr("test.io", "v1", "widgets", None);
    // The CRD first.
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
                    "schema": {"openAPIV3Schema": {"type": "object", "properties": {
                        "spec": {"type": "object", "x-kubernetes-preserve-unknown-fields": true},
                        "status": {"type": "object", "x-kubernetes-preserve-unknown-fields": true}}}}}]
            }
        }"#
            .to_owned(),
        ),
    )
    .await;
    assert_eq!(code, 201, "the CRD registers");

    let listed = Face::list(&face, &target).await.unwrap();
    let rv0 = listed.resource_version.clone();

    // Watch from the pre-create rv.
    let mut rx = Face::watch(&face, &target, rv0.as_deref()).await.unwrap();

    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/test.io/v1/namespaces/default/widgets",
        Some(r#"{"apiVersion": "test.io/v1", "kind": "Widget", "metadata": {"name": "probe"}, "spec": {}}"#.to_owned()),
    )
    .await;
    assert_eq!(code, 201, "the widget applies");

    // The feed's leading bookmark first; then the Added.
    let added = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(krapi::face::WatchEvent::Bookmark(_)) => {}
                Some(krapi::face::WatchEvent::Added(b)) => break b,
                other => panic!("unexpected event: {other:?}"),
            }
        }
    })
    .await;
    let Ok(b) = added else {
        panic!("the Added never arrived: {added:?}");
    };
    assert_eq!(
        b.pointer("/metadata/name")
            .and_then(serde_json::Value::as_str),
        Some("probe")
    );

    let (code, _) = http_json(
        "DELETE",
        &url,
        "/apis/test.io/v1/namespaces/default/widgets/probe",
        None,
    )
    .await;
    assert_eq!(code, 200, "the delete lands");

    let deleted = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(krapi::face::WatchEvent::Bookmark(_)) => {}
                Some(krapi::face::WatchEvent::Deleted(b)) => break b,
                other => panic!("unexpected event: {other:?}"),
            }
        }
    })
    .await;
    match deleted {
        Ok(b) => {
            assert_eq!(
                b.pointer("/metadata/name")
                    .and_then(serde_json::Value::as_str),
                Some("probe"),
                "the tombstone names the dead row"
            );
        }
        other => panic!("the Deleted never arrived: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_metadata_patch_keeps_the_spec() {
    let url = server().await;
    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/apiextensions.k8s.io/v1/customresourcedefinitions",
        Some(
            r#"{
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "stores.test.io"},
            "spec": {
                "group": "test.io", "scope": "Namespaced",
                "names": {"plural": "stores", "singular": "store", "kind": "Store", "listKind": "StoreList"},
                "versions": [{"name": "v1", "served": true, "storage": true,
                    "subresources": {"status": {}},
                    "schema": {"openAPIV3Schema": {"type": "object", "properties": {
                        "spec": {"type": "object", "properties": {
                            "name": {"type": "string"}},
                            "x-kubernetes-preserve-unknown-fields": false},
                        "status": {"type": "object", "x-kubernetes-preserve-unknown-fields": true}}}}}]
            }
        }"#
            .to_owned(),
        ),
    )
    .await;
    assert_eq!(code, 201, "the CRD registers");

    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/test.io/v1/namespaces/default/stores",
        Some(r#"{"apiVersion": "test.io/v1", "kind": "Store", "metadata": {"name": "s1", "finalizers": ["x/f"]}, "spec": {"name": "kept"}}"#.to_owned()),
    )
    .await;
    assert_eq!(code, 201, "the row applies");

    // The metadata-only merge patch (the finalizer drop shape).
    let (code, _) = http_json(
        "PATCH",
        &url,
        "/apis/test.io/v1/namespaces/default/stores/s1",
        Some(r#"{"metadata": {"finalizers": []}}"#.to_owned()),
    )
    .await;
    assert_eq!(code, 200, "the patch lands: {code}");

    let (code, body) = http_json(
        "GET",
        &url,
        "/apis/test.io/v1/namespaces/default/stores/s1",
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        body.pointer("/spec/name"),
        Some(&serde_json::json!("kept")),
        "the spec survives a metadata-only patch: {body}"
    );
}
