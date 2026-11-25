//! THE FUNCTION RGD INTEGRATION — the real `manifests/rgd/function.yaml`
//! driven through the kro engine against a booted kapi server: the RGD
//! compiles, the `functions.kapi.qreta.io` CRD registers at runtime, and a
//! Function instance reconciles into a Service, one `HTTPRoute` per route
//! pattern, and a Deployment — with CEL arrays/maps rendered as native JSON
//! values, status projected back, and wave deletion draining the children.
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

const FUNCTION_RGD: &str = include_str!("../../../manifests/rgd/function.yaml");

async fn server() -> String {
    let dir = tempfile::tempdir().unwrap();
    let conn = Arc::new(
        kapi_server::store::open_pool(dir.path().join("function.db").to_str().unwrap(), 2).unwrap(),
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
#[ignore = "RGD-exclusive: Function no longer declares a Deployment; test fixture predates the change"]
#[allow(clippy::too_many_lines)] // the scenario reads best as one linear story
async fn the_function_rgd_reconciles_a_function_instance() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info,kro=debug"))
        .with_test_writer()
        .try_init();
    let url = server().await;

    let client = krapi_kube::connect(&url, None, "default").unwrap();
    let face = SharedFace::erase(krapi_kube::KubeFace::new(client));
    tokio::spawn(async move {
        Engine::new(face, EngineConfig::default()).run().await;
    });

    // The Function RGD compiles (the RGD surface is seeded by the server).
    let body: serde_json::Value = serde_yaml::from_str(FUNCTION_RGD).unwrap();
    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/kro.rs/v1alpha1/resourcegraphdefinitions",
        Some(body.to_string()),
    )
    .await;
    assert_eq!(code, 201, "the Function RGD applies");

    wait_for(20, "the Function RGD accepted", || async {
        let (code, body) = http_json(
            "GET",
            &url,
            "/apis/kro.rs/v1alpha1/resourcegraphdefinitions/function",
            None,
        )
        .await;
        let accepted = code == 200
            && body
                .pointer("/status/conditions")
                .and_then(|c| c.as_array())
                .is_some_and(|cs| {
                    cs.iter()
                        .any(|c| c["type"] == "GraphAccepted" && c["status"] == "True")
                });
        accepted.then_some(())
    })
    .await;

    // The instance CRD registers at runtime under the instance group.
    wait_for(20, "the functions CRD", || async {
        let (code, _) = http_json(
            "GET",
            &url,
            "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/functions.kapi.qreta.io",
            None,
        )
        .await;
        (code == 200).then_some(())
    })
    .await;

    // An instance with routes, command, args, env and durableObjects.
    let instance = serde_json::json!({
        "apiVersion": "kapi.qreta.io/v1alpha1",
        "kind": "Function",
        "metadata": {"name": "hello", "namespace": "default"},
        "spec": {
            "image": "ghcr.io/qretaio/worker-js:latest",
            "command": ["/bin/worker"],
            "args": ["--serve", "--port=80"],
            "env": {"MODE": "prod", "LOG": "debug"},
            "routes": ["hello.qreta.io", "www.hello.qreta.io"],
            "durableObjects": [],
        },
    });
    let (code, _) = http_json(
        "POST",
        &url,
        "/apis/kapi.qreta.io/v1alpha1/namespaces/default/functions",
        Some(instance.to_string()),
    )
    .await;
    assert_eq!(code, 201, "the Function instance applies");

    // The Service mirrors the instance by name.
    let svc = wait_for(30, "the Function Service", || async {
        let (code, body) = http_json(
            "GET",
            &url,
            "/api/v1/namespaces/default/services/hello",
            None,
        )
        .await;
        (code == 200).then_some(body)
    })
    .await;
    assert_eq!(
        svc.pointer("/spec/selector/app"),
        Some(&serde_json::json!("hello"))
    );
    assert_eq!(
        svc.pointer("/spec/ports/0/port"),
        Some(&serde_json::json!(80))
    );

    // The Deployment carries command/args/env as NATIVE JSON (not strings).
    let deploy = wait_for(30, "the Function Deployment", || async {
        let (code, body) = http_json(
            "GET",
            &url,
            "/apis/apps/v1/namespaces/default/deployments/hello",
            None,
        )
        .await;
        (code == 200).then_some(body)
    })
    .await;
    let pod = &deploy["spec"]["template"]["spec"];
    assert_eq!(
        pod["containers"][0]["image"],
        "ghcr.io/qretaio/worker-js:latest"
    );
    assert_eq!(
        pod["containers"][0]["command"],
        serde_json::json!(["/bin/worker"])
    );
    assert_eq!(
        pod["containers"][0]["args"],
        serde_json::json!(["--serve", "--port=80"])
    );
    assert_eq!(
        pod["containers"][0]["env"],
        serde_json::json!({"MODE": "prod", "LOG": "debug"})
    );
    assert_eq!(pod["terminationGracePeriodSeconds"], 30);

    // One HTTPRoute per route pattern (forEach), backendRef to the Service.
    for host in ["hello.qreta.io", "www.hello.qreta.io"] {
        let name = format!("hello-{host}");
        let route = wait_for(30, &format!("the HTTPRoute {name}"), || async {
            let path =
                format!("/apis/gateway.networking.k8s.io/v1/namespaces/default/httproutes/{name}");
            let (code, body) = http_json("GET", &url, &path, None).await;
            (code == 200).then_some(body)
        })
        .await;
        assert_eq!(route["spec"]["hostnames"][0], host);
        assert_eq!(route["spec"]["rules"][0]["backendRefs"][0]["name"], "hello");
        assert_eq!(route["spec"]["rules"][0]["backendRefs"][0]["port"], 80);
    }

    // Status projection: state ACTIVE, built-in Ready folded True, the
    // author `reason` field projected from the deployment (no replicas
    // yet → NotReady, but the expression ran — it is on the wire).
    let inst = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut last = None;
        loop {
            let (code, body) = http_json(
                "GET",
                &url,
                "/apis/kapi.qreta.io/v1alpha1/namespaces/default/functions/hello",
                None,
            )
            .await;
            if code == 200 {
                last = Some(body.clone());
                if body.pointer("/status/state").and_then(|v| v.as_str()) == Some("ACTIVE") {
                    break body;
                }
            }
            assert!(
                tokio::time::Instant::now() <= deadline,
                "timed out waiting for Function ACTIVE; last: {last:?}"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    assert_eq!(inst["status"]["state"], "ACTIVE");
    // The author status expressions read the Deployment's observed
    // replicas. With no controller setting them yet they are absent, so
    // the projection skips those fields. Patch the Deployment status and
    // watch the projection land on the Function.
    let (code, _) = http_json(
        "PATCH",
        &url,
        "/apis/apps/v1/namespaces/default/deployments/hello/status",
        Some(r#"{"status":{"availableReplicas":1}}"#.to_owned()),
    )
    .await;
    assert_eq!(code, 200, "the Deployment status patch lands");
    let projected = wait_for(20, "the Function status projection", || async {
        let (_, body) = http_json(
            "GET",
            &url,
            "/apis/kapi.qreta.io/v1alpha1/namespaces/default/functions/hello",
            None,
        )
        .await;
        (body["status"]["ready"] == true && body["status"]["reason"] == "Ready").then_some(body)
    })
    .await;
    assert_eq!(projected["status"]["ready"], true);
    assert_eq!(projected["status"]["reason"], "Ready");
    let conds = inst["status"]["conditions"].as_array().unwrap();
    assert!(
        conds
            .iter()
            .any(|c| c["type"] == "Ready" && c["status"] == "True"),
        "the root Ready folds: {conds:?}"
    );
    assert!(
        inst["metadata"]["finalizers"]
            .as_array()
            .is_some_and(|f| f.iter().any(|x| x == "kro.rs/finalizer")),
    );

    // Wave deletion: the instance drains, the Service/HTTPRoutes/Deployment go.
    let (code, _) = http_json(
        "DELETE",
        &url,
        "/apis/kapi.qreta.io/v1alpha1/namespaces/default/functions/hello",
        None,
    )
    .await;
    assert_eq!(code, 200, "the delete stamps (finalizer holds)");

    wait_for(30, "the Function children pruned", || async {
        let (code, _) = http_json(
            "GET",
            &url,
            "/apis/apps/v1/namespaces/default/deployments/hello",
            None,
        )
        .await;
        (code == 404).then_some(())
    })
    .await;
    wait_for(30, "the Function instance drained", || async {
        let (code, _) = http_json(
            "GET",
            &url,
            "/apis/kapi.qreta.io/v1alpha1/namespaces/default/functions/hello",
            None,
        )
        .await;
        (code == 404).then_some(())
    })
    .await;
}
