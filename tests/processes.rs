mod support;
use axum::{Router, routing::get};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
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
async fn read(client: &reqwest::Client, url: &str) -> Value {
    client.get(url).send().await.unwrap().json().await.unwrap()
}
async fn wait(client: &reqwest::Client, base: &str, id: &str) -> Value {
    for _ in 0..200 {
        let value = read(client, &format!("{base}/v1/tasks/{id}")).await;
        if value["state"] != "running" && value["state"] != "queued" {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("task hung")
}
#[tokio::test]
async fn worker_timeout_cancel_kill_process_tree_and_release_budget() {
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    let pid_file = dir.path().join("pids");
    // Interpose only the external worker launcher, after actual catalog/download.
    // The shell and its descendant retain the real service-created process group.
    let launcher = tools.join("prlimit");
    std::fs::write(&launcher, "#!/bin/sh\nsleep 60 &\nprintf '%s %s' \"$$\" \"$!\" > \"$TEST_PID_FILE\"\nhead -c 1048576 /dev/zero >&2\nprintf 'private-worker-marker' >&2\nwait\n").unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (catalog, payload) = support::fixture();
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
        )
        .route(
            "/asset/Android/fixture.bundle",
            get(move || {
                let p = payload.clone();
                async { p }
            }),
        );
    let fixture = tokio::spawn(async move { axum::serve(fixture, routes).await.unwrap() });
    let address = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = address.local_addr().unwrap();
    drop(address);
    let data_dir = dir.path().join("data");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, format!("listen=\"{addr}\"\ndata_dir=\"{}\"\ncdn_root=\"http://{origin}\"\nallow_loopback_http=true\nworker_timeout_secs=1\n",data_dir.display())).unwrap();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_moenotes-assets"))
            .arg("serve")
            .arg(config)
            .env(
                "PATH",
                format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
            )
            .env("TEST_PID_FILE", &pid_file)
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
    let refreshed: Value = client
        .post(format!("{base}/v1/catalogs/refresh"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        wait(&client, &base, refreshed["id"].as_str().unwrap()).await["state"],
        "succeeded"
    );
    for cancel in [false, true] {
        let task: Value = client
            .post(format!("{base}/v1/exports"))
            .json(&json!({"keys":[support::KEY]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        for _ in 0..100 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pids = std::fs::read_to_string(&pid_file).unwrap();
        std::fs::remove_file(&pid_file).unwrap();
        if cancel {
            client
                .post(format!(
                    "{base}/v1/tasks/{}/cancel",
                    task["id"].as_str().unwrap()
                ))
                .send()
                .await
                .unwrap();
        }
        let task = wait(&client, &base, task["id"].as_str().unwrap()).await;
        assert_eq!(task["state"], if cancel { "cancelled" } else { "failed" });
        assert_eq!(task["completed"], 1);
        assert!(!task.to_string().contains("private-worker-marker"));
        if !cancel {
            assert!(
                task["results"][0]["error"]
                    .as_str()
                    .unwrap()
                    .starts_with("worker_timeout")
            );
        }
        for _ in 0..100 {
            if read(&client, &format!("{base}/readyz")).await["temporary_reserved_bytes"] == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            read(&client, &format!("{base}/readyz")).await["temporary_reserved_bytes"],
            0
        );
        assert_eq!(std::fs::read_dir(data_dir.join("tmp")).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_dir(data_dir.join("exports")).unwrap().count(),
            0
        );
        for pid in pids.split_whitespace() {
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
                assert!(
                    status
                        .lines()
                        .any(|l| l.starts_with("State:") && l.contains('Z')),
                    "descendant still executing: {pid}"
                );
            }
        }
    }
    fixture.abort();
}
