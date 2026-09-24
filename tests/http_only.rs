mod support;
use axum::{Router, routing::get};
use moenotes_assets::{
    catalog::{Catalog, Options},
    config::Config,
    service::Snapshot,
};
use serde_json::{Value, json};
use std::{
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn task(c: &reqwest::Client, base: &str, id: &str) -> Value {
    for _ in 0..200 {
        let t: Value = c
            .get(format!("{base}/v1/tasks/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if t["state"] != "queued" && t["state"] != "running" {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task did not finish")
}
#[tokio::test]
async fn http_only_preview_font_retention_archive_and_missing_object() {
    let (bytes, payload) = support::fixture();
    let mut catalog = Catalog::parse(&bytes).unwrap();
    let target = catalog.target(support::KEY).unwrap().clone();
    let remote = catalog.closure(support::KEY).unwrap().remove(0);
    let mut local = remote.clone();
    local.id = 900001;
    local.key = "local-script".into();
    local.internal = "{RuntimePath}/shared_monoscripts.bundle".into();
    local.options = Some(Options {
        size: 999,
        crc: 123,
        ..remote.options.clone().unwrap()
    });
    catalog.locations.insert(local.id, local.clone());
    catalog
        .locations
        .get_mut(&target.id)
        .unwrap()
        .dependencies
        .push(local.id);
    for (id, key, kind, deps, internal) in [
        (
            900002,
            "Font/remote",
            "TMPro.TMP_FontAsset",
            vec![remote.id, local.id],
            support::INTERNAL,
        ),
        (
            900003,
            "Font/package",
            "TMPro.TMP_FontAsset",
            vec![local.id],
            "Assets/font.asset",
        ),
        (
            900004,
            "Image/missing",
            "UnityEngine.Texture2D",
            vec![remote.id, local.id],
            "Assets/missing.asset",
        ),
    ] {
        let mut l = target.clone();
        l.id = id;
        l.key = key.into();
        l.resource_type = kind.into();
        l.dependencies = deps;
        l.internal = internal.into();
        catalog.locations.insert(id, l);
        catalog.keys.insert(key.into(), vec![id]);
    }
    catalog.keys.insert(remote.key.clone(), vec![remote.id]);
    let hits = Arc::new(AtomicUsize::new(0));
    let fixture = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = fixture.local_addr().unwrap();
    let routes = Router::new().route(
        "/asset/Android/fixture.bundle",
        get({
            let hits = hits.clone();
            move || {
                let b = payload.clone();
                hits.fetch_add(1, Ordering::SeqCst);
                async move { b }
            }
        }),
    );
    let fixture = tokio::spawn(async move { axum::serve(fixture, routes).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(data.join("index.sqlite"))
                .create_if_missing(true),
        )
        .await
        .unwrap();
    sqlx::query("CREATE TABLE snapshots(id TEXT PRIMARY KEY,body TEXT NOT NULL,catalog TEXT NOT NULL,current INTEGER NOT NULL DEFAULT 0)").execute(&db).await.unwrap();
    let snapshot = Snapshot {
        id: "snapshot".into(),
        content_sha256: "fixture".into(),
        region: "test".into(),
        locale: "".into(),
        bili_version: "main".into(),
        cdn_root: format!("http://{origin}"),
        remote_hash: "fixture".into(),
        created: 0,
    };
    sqlx::query("INSERT INTO snapshots VALUES(?,?,?,1)")
        .bind(&snapshot.id)
        .bind(serde_json::to_string(&snapshot).unwrap())
        .bind(serde_json::to_string(&catalog).unwrap())
        .execute(&db)
        .await
        .unwrap();
    db.close().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        toml::to_string(&Config {
            listen: addr,
            data_dir: data.clone(),
            cdn_root: format!("http://{origin}"),
            allow_loopback_http: true,
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_moenotes-assets"))
            .arg("serve")
            .arg(config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let c = reqwest::Client::new();
    let base = format!("http://{addr}");
    for _ in 0..200 {
        if c.get(format!("{base}/readyz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let selection = json!({"keys":[support::KEY,"Font/remote","Font/package","Image/missing"]});
    let preflight: Value = c
        .post(format!("{base}/v1/preflight"))
        .json(&selection)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for row in preflight["results"].as_array().unwrap() {
        assert_eq!(
            row["omitted_local_dependencies"].as_array().unwrap().len(),
            1
        );
        assert!(
            row["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .all(|d| d["status"] == "remote")
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let answer: Value = c
        .post(format!("{base}/v1/exports"))
        .json(&selection)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let result = task(&c, &base, answer["id"].as_str().unwrap()).await;
    assert_eq!(result["state"], "partial", "{result}");
    assert_eq!(result["completed"], 4);
    for row in result["results"].as_array().unwrap() {
        let key = row["key"].as_str().unwrap();
        if key == "Image/missing" {
            assert!(
                row["error"]
                    .as_str()
                    .unwrap()
                    .contains("not found in bundle")
            );
            continue;
        }
        assert!(row["error"].is_null(), "{row}");
        let m: Value = c
            .get(format!(
                "{base}/v1/exports/{}",
                row["export_id"].as_str().unwrap()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            m["sources"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["internal_id"].as_str().unwrap().starts_with("https://"))
        );
        assert_eq!(m["options"]["dependency_closure_complete"], false);
        if key == support::KEY {
            assert_eq!(m["files"].as_array().unwrap().len(), 1);
            assert_eq!(m["options"]["disposition"], "preview");
        } else {
            let files = m["files"].as_array().unwrap();
            assert_eq!(files.len(), if key == "Font/package" { 1 } else { 2 });
            assert_eq!(files[0]["metadata"]["role"], "font-reference");
            let b: Value = c
                .get(format!(
                    "{base}/v1/files/{}",
                    files[0]["id"].as_str().unwrap()
                ))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(b["package_payload_downloaded"], false);
            if key == "Font/remote" {
                assert_eq!(files[1]["media_type"], "application/vnd.unity");
            }
        }
    }
    let archive: Value = c
        .post(format!("{base}/v1/exports"))
        .json(&json!({"keys":[support::KEY],"archive":true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        task(&c, &base, archive["id"].as_str().unwrap()).await["state"],
        "succeeded"
    );
    let resources: Value = c
        .get(format!("{base}/v1/resources"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resources["total"], 1);
    let all: Value = c
        .post(format!("{base}/v1/resources/archive"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let all = task(&c, &base, all["id"].as_str().unwrap()).await;
    assert_eq!(all["state"], "succeeded");
    assert_eq!(all["total"], 1);
    assert!(hits.load(Ordering::SeqCst) > 0);
    fixture.abort();
}
