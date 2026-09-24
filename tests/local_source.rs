mod support;
use axum::{Router, routing::get};
use moenotes_assets::{
    config::Config,
    local::{Entry, Source},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    process::{Command, Stdio},
    time::Duration,
};
struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn task(client: &reqwest::Client, base: &str, id: &str) -> Value {
    for _ in 0..200 {
        let r: Value = client
            .get(format!("{base}/v1/tasks/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if r["state"] != "queued" && r["state"] != "running" {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task hung")
}
#[tokio::test]
async fn pinned_local_provider_works_and_rejects_changed_bytes() {
    let root = tempfile::tempdir().unwrap();
    let sources = root.path().join("source");
    std::fs::create_dir(&sources).unwrap();
    let (_, raw) = support::fixture();
    std::fs::write(sources.join("fixture.bundle"), &raw).unwrap();
    let (_, crc) = support::bundle();
    let internal = "{RuntimePath}/Android/fixture.bundle";
    let catalog = support::catalog_with_internal(raw.len() as u64, crc, internal);
    let fixture = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = fixture.local_addr().unwrap();
    let routes = Router::new()
        .route(
            "/asset/Android/catalog_main_zh-Hant.hash",
            get(|| async { "fixture" }),
        )
        .route(
            "/asset/Android/catalog_main_zh-Hant.bin",
            get(move || {
                let c = catalog.clone();
                async { c }
            }),
        );
    let fixture = tokio::spawn(async move { axum::serve(fixture, routes).await.unwrap() });
    for corrupt in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        if corrupt {
            let mut altered = raw.clone();
            altered[25] ^= 1;
            std::fs::write(sources.join("fixture.bundle"), altered).unwrap();
        }
        let c = Config {
            listen: addr,
            cdn_root: format!("http://{origin}"),
            allow_loopback_http: true,
            data_dir: root.path().join(format!("data-{corrupt}")),
            local_source: Some(Source {
                root: sources.clone(),
                entries: BTreeMap::from([(
                    internal.into(),
                    Entry {
                        path: "fixture.bundle".into(),
                        sha256: moenotes_assets::crypto::digest(&raw),
                        bytes: raw.len() as u64,
                    },
                )]),
            }),
            ..Default::default()
        };
        let config = root.path().join(format!("{corrupt}.toml"));
        std::fs::write(&config, toml::to_string(&c).unwrap()).unwrap();
        let _server = Server(
            Command::new(env!("CARGO_BIN_EXE_moenotes-assets"))
                .arg("serve")
                .arg(config)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");
        for _ in 0..100 {
            if client
                .get(format!("{base}/readyz"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let refresh: Value = client
            .post(format!("{base}/v1/catalogs/refresh"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            task(&client, &base, refresh["id"].as_str().unwrap()).await["state"],
            "succeeded"
        );
        let preflight: Value = client
            .post(format!("{base}/v1/preflight"))
            .json(&json!({"keys":[support::KEY]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(preflight["results"][0]["status"], "local");
        let export: Value = client
            .post(format!("{base}/v1/exports"))
            .json(&json!({"keys":[support::KEY]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let export = task(&client, &base, export["id"].as_str().unwrap()).await;
        if corrupt {
            assert_eq!(export["state"], "failed");
            assert!(
                export["results"][0]["error"]
                    .as_str()
                    .unwrap()
                    .contains("SHA256")
            );
        } else {
            assert_eq!(export["state"], "succeeded", "{export}");
        }
    }
    fixture.abort();
}
