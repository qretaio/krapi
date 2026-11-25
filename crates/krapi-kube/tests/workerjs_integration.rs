//! THE WORKERJS RGD INTEGRATION: a `WorkerJs` instance REUSES a shared
//! Function (`functionRef`) and only adds `HTTPRoute`s to its Service.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use krapi::face::SharedFace;
use krapi::{Engine, EngineConfig};
use std::sync::Arc;
use std::time::Duration;

const WORKERJS_RGD: &str = include_str!("../../../manifests/rgd/workerjs.yaml");
const FUNCTION_RGD: &str = include_str!("../../../manifests/rgd/function.yaml");

async fn server() -> String {
    let dir = tempfile::tempdir().unwrap();
    let conn = Arc::new(
        kapi_server::store::open_pool(dir.path().join("workerjs.db").to_str().unwrap(), 2).unwrap(),
    );
    kapi_server::seed_inbuilt_crds(&conn).unwrap();
    let router = kapi_server::api::router(Arc::new(kapi_server::api::AppState::new(conn, None)));
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
#[allow(clippy::too_many_lines)]
async fn workerjs_reuses_a_function_and_adds_routes() {
    let url = server().await;
    let client = krapi_kube::connect(&url, None, "default").unwrap();
    let face = SharedFace::erase(krapi_kube::KubeFace::new(client));
    tokio::spawn(async move {
        Engine::new(face, EngineConfig::default()).run().await;
    });

    for manifest in [FUNCTION_RGD, WORKERJS_RGD] {
        let body: serde_json::Value = serde_yaml::from_str(manifest).unwrap();
        let (code, _) = http_json(
            "POST",
            &url,
            "/apis/kro.rs/v1alpha1/resourcegraphdefinitions",
            Some(body.to_string()),
        )
        .await;
        assert_eq!(code, 201, "the RGD applies");
    }
    wait_for(20, "the workerjsapps CRD", || async {
        let (code, _) = http_json(
            "GET",
            &url,
            "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/workerjsapps.kapi.qreta.io",
            None,
        )
        .await;
        (code == 200).then_some(())
    })
    .await;

    // The shared Function instance reused via functionRef.
    let shared = serde_json::json!({
        "apiVersion": "kapi.qreta.io/v1alpha1",
        "kind": "Function",
        "metadata": {"name": "wisolate", "namespace": "default"},
        "spec": {"image": "ghcr.io/qretaio/wisolate:latest", "routes": []},
    });
    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/kapi.qreta.io/v1alpha1/namespaces/default/functions",
        Some(shared.to_string()),
    )
    .await;
    assert_eq!(code, 201, "the shared Function applies");

    let instance = serde_json::json!({
        "apiVersion": "kapi.qreta.io/v1alpha1",
        "kind": "WorkerJs",
        "metadata": {"name": "hello", "namespace": "default"},
        "spec": {"functionRef": "wisolate", "routes": ["qreta.io/games/"], "image": "ghcr.io/qretaio/games:latest"},
    });
    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/kapi.qreta.io/v1alpha1/namespaces/default/workerjsapps",
        Some(instance.to_string()),
    )
    .await;
    assert_eq!(code, 201, "the WorkerJs instance applies");

    // The only child: a route to the shared Function's Service.
    let route = wait_for(30, "the HTTPRoute", || async {
        let (code, body) = http_json(
            "GET",
            &url,
            "/apis/gateway.networking.k8s.io/v1/namespaces/default/httproutes/hello-qreta.io-games",
            None,
        )
        .await;
        (code == 200).then_some(body)
    })
    .await;
    assert_eq!(route["spec"]["hostnames"][0], "qreta.io");
    assert_eq!(
        route["spec"]["rules"][0]["backendRefs"][0]["name"],
        "wisolate"
    );
}
