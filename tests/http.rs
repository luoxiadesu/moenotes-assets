mod support;
use axum::{Router, body::Body, http::Response, routing::get};
use serde_json::{Value, json};
use std::{
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::*;

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn task(client: &reqwest::Client, base: &str, id: &str) -> Value {
    for _ in 0..200 {
        let t: Value = client
            .get(format!("{base}/v1/tasks/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if t["state"] != "running" && t["state"] != "queued" {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("task timed out")
}
async fn post(client: &reqwest::Client, url: String, body: Value) -> Value {
    let r = client.post(url).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 202);
    r.json().await.unwrap()
}

#[tokio::test]
async fn http_pipeline_dedupe_cancel_recovery_and_range() {
    let (cat, payload) = fixture();
    let count = Arc::new(AtomicUsize::new(0));
    let mode = Arc::new(AtomicUsize::new(0));
    let routes = Router::new()
        .route(
            "/asset/Android/catalog_main_zh-Hant.hash",
            get(|| async { "fixture-hash" }),
        )
        .route(
            "/asset/Android/catalog_main_zh-Hant.bin",
            get(move || {
                let cat = cat.clone();
                async move { cat }
            }),
        )
        .route(
            "/asset/Android/fixture.bundle",
            get({
                let count = count.clone();
                let mode = mode.clone();
                move || {
                    let p = payload.clone();
                    let count = count.clone();
                    let mode = mode.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        match mode.load(Ordering::SeqCst) {
                            1 => {
                                tokio::time::sleep(Duration::from_secs(3)).await;
                            }
                            2 => {
                                return Response::builder()
                                    .status(200)
                                    .body(Body::from(vec![0; p.len()]))
                                    .unwrap();
                            }
                            3 => {
                                return Response::builder()
                                    .status(302)
                                    .header("Location", "http://127.0.0.1:1/not-followed")
                                    .body(Body::empty())
                                    .unwrap();
                            }
                            _ => {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                        Response::new(Body::from(p))
                    }
                }
            }),
        );
    let fixture = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = fixture.local_addr().unwrap();
    let fixture_task = tokio::spawn(async move { axum::serve(fixture, routes).await.unwrap() });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config,format!("listen=\"{addr}\"\ndata_dir=\"{}/data\"\ncdn_root=\"http://{origin}\"\nallow_loopback_http=true\nqueue_limit=2\n",dir.path().display())).unwrap();
    let start = || {
        Server(
            Command::new(env!("CARGO_BIN_EXE_moenotes-assets"))
                .arg("serve")
                .arg(&config)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let mut server = start();
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    async fn ready(c: &reqwest::Client, b: &str) {
        for _ in 0..100 {
            if c.get(format!("{b}/readyz"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("service not ready")
    }
    ready(&client, &base).await;
    let r = post(&client, format!("{base}/v1/catalogs/refresh"), json!({})).await;
    assert_eq!(
        task(&client, &base, r["id"].as_str().unwrap()).await["state"],
        "succeeded"
    );
    mode.store(1, Ordering::SeqCst);
    let cancelled = post(&client, format!("{base}/v1/exports"), json!({"keys":[KEY]})).await;
    for _ in 0..100 {
        if count.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client
        .post(format!(
            "{base}/v1/tasks/{}/cancel",
            cancelled["id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        task(&client, &base, cancelled["id"].as_str().unwrap()).await["state"],
        "cancelled"
    );
    let slow_a = post(&client, format!("{base}/v1/exports"), json!({"keys":[KEY]})).await;
    let slow_b = post(&client, format!("{base}/v1/exports"), json!({"keys":[KEY]})).await;
    let rejected = client
        .post(format!("{base}/v1/exports"))
        .json(&json!({"keys":[KEY]}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 429);
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    server = start();
    ready(&client, &base).await;
    for slow in [slow_a, slow_b] {
        let t = task(&client, &base, slow["id"].as_str().unwrap()).await;
        assert_eq!(t["state"], "failed");
        assert!(t["error"].as_str().unwrap().contains("interrupted"));
        assert_eq!(t["completed"], t["total"]);
        assert_eq!(t["results"].as_array().unwrap().len(), 1);
        assert_eq!(t["results"][0]["key"], KEY);
    }
    mode.store(2, Ordering::SeqCst);
    let bad = post(&client, format!("{base}/v1/exports"), json!({"keys":[KEY]})).await;
    assert_eq!(
        task(&client, &base, bad["id"].as_str().unwrap()).await["state"],
        "failed"
    );
    mode.store(3, Ordering::SeqCst);
    let redirect = post(&client, format!("{base}/v1/exports"), json!({"keys":[KEY]})).await;
    let redirect = task(&client, &base, redirect["id"].as_str().unwrap()).await;
    assert_eq!(redirect["state"], "failed");
    assert!(
        redirect["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("302")
    );
    mode.store(0, Ordering::SeqCst);
    let previous = count.load(Ordering::SeqCst);
    let one = post(&client, format!("{base}/v1/exports"), json!({"keys":[KEY]})).await;
    let two = post(&client, format!("{base}/v1/exports"), json!({"keys":[KEY]})).await;
    let one = task(&client, &base, one["id"].as_str().unwrap()).await;
    let two = task(&client, &base, two["id"].as_str().unwrap()).await;
    assert_eq!(one["state"], "succeeded", "{one}");
    assert_eq!(two["state"], "succeeded");
    assert_eq!(count.load(Ordering::SeqCst), previous + 1);
    let eid = one["results"][0]["export_id"].as_str().unwrap();
    let manifest: Value = client
        .get(format!("{base}/v1/exports/{eid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fid = manifest["files"][0]["id"].as_str().unwrap();
    let url = format!("{base}/v1/files/{fid}");
    let full = client.get(&url).send().await.unwrap();
    let etag = full.headers()["etag"].clone();
    assert_eq!(full.bytes().await.unwrap().as_ref(), BODY);
    let range = client
        .get(&url)
        .header("Range", "bytes=0-3")
        .send()
        .await
        .unwrap();
    assert_eq!(range.status(), 206);
    assert_eq!(range.bytes().await.unwrap().as_ref(), &BODY[..4]);
    assert_eq!(
        client
            .get(&url)
            .header("If-None-Match", etag.clone())
            .send()
            .await
            .unwrap()
            .status(),
        304
    );
    assert_eq!(
        client
            .get(&url)
            .header("If-None-Match", format!("W/{}", etag.to_str().unwrap()))
            .send()
            .await
            .unwrap()
            .status(),
        304
    );
    let unsatisfied = client
        .get(&url)
        .header("Range", "bytes=9999999-")
        .send()
        .await
        .unwrap();
    assert_eq!(unsatisfied.status(), 416);
    assert_eq!(unsatisfied.headers()["cache-control"], "no-store");
    let fallback = client
        .get(&url)
        .header("Range", "bytes=0-3")
        .header("If-Range", "\"old\"")
        .send()
        .await
        .unwrap();
    assert_eq!(fallback.status(), 200);
    assert_eq!(fallback.bytes().await.unwrap().as_ref(), BODY);
    assert!(
        client
            .head(&url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .is_empty()
    );
    let mixed = post(
        &client,
        format!("{base}/v1/exports"),
        json!({"keys":[KEY,"missing"]}),
    )
    .await;
    assert_eq!(
        task(&client, &base, mixed["id"].as_str().unwrap()).await["state"],
        "partial"
    );
    assert_eq!(count.load(Ordering::SeqCst), previous + 1);
    // Simulate an interrupted process and a stale temporary file, then reuse the committed result.
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    std::fs::write(dir.path().join("data/tmp/orphan"), b"temporary").unwrap();
    server = start();
    ready(&client, &base).await;
    assert!(!dir.path().join("data/tmp/orphan").exists());
    assert_eq!(client.get(&url).send().await.unwrap().status(), 200);
    let repeat = post(&client, format!("{base}/v1/exports"), json!({"keys":[KEY]})).await;
    assert_eq!(
        task(&client, &base, repeat["id"].as_str().unwrap()).await["state"],
        "succeeded"
    );
    assert_eq!(count.load(Ordering::SeqCst), previous + 1);
    let preflight: Value = client
        .post(format!("{base}/v1/preflight"))
        .json(&json!({"keys":[KEY,"missing"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preflight["results"][0]["status"], "remote");
    assert_eq!(preflight["results"][1]["status"], "missing");
    let tree_root = dir.path().join("tree");
    let index = moenotes_assets::tree::export(&dir.path().join("data"), &tree_root, None, false)
        .await
        .unwrap();
    assert_eq!(index.objects.len(), 1);
    assert!(index.objects[0].object_key.starts_with(KEY));
    assert!(!tree_root.join(&index.objects[0].object_key).is_symlink());
    let repeated = moenotes_assets::tree::export(&dir.path().join("data"), &tree_root, None, false)
        .await
        .unwrap();
    assert_eq!(index.objects, repeated.objects);
    let public = std::fs::read_to_string(tree_root.join("_meta/manifest.json")).unwrap();
    assert!(!public.contains(&dir.path().display().to_string()));
    std::fs::write(tree_root.join(&index.objects[0].object_key), b"conflict").unwrap();
    assert!(
        moenotes_assets::tree::export(&dir.path().join("data"), &tree_root, None, false)
            .await
            .is_err()
    );
    let file_path = dir
        .path()
        .join("data/exports")
        .join(eid)
        .join(manifest["files"][0]["name"].as_str().unwrap());
    std::fs::remove_file(file_path).unwrap();
    assert_eq!(
        client
            .get(&url)
            .header("If-None-Match", etag)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    drop(server);
    fixture_task.abort();
}
