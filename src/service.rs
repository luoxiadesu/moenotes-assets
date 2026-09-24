use crate::{
    catalog::{CRYPT, Catalog, Location, Selector},
    config::Config,
    crypto, diagnostics,
    shared::{self, Registry},
    task_runner,
    worker::{self, Artifact, Input, Job},
};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as Param, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub content_sha256: String,
    pub region: String,
    pub locale: String,
    pub bili_version: String,
    pub cdn_root: String,
    pub remote_hash: String,
    pub created: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct PublishedFile {
    #[serde(flatten)]
    pub artifact: Artifact,
    pub id: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub snapshot: String,
    pub key: String,
    pub profile: String,
    pub files: Vec<PublishedFile>,
    pub sources: Vec<Value>,
    #[serde(default)]
    pub selected_location: Option<u32>,
    #[serde(default)]
    pub empty: bool,
    #[serde(default)]
    pub options: Value,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ItemResult {
    pub key: String,
    pub export_id: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub snapshot: Option<String>,
    /// Original selection, retained so interrupted tasks can account for every key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    pub total: usize,
    pub completed: usize,
    pub results: Vec<ItemResult>,
    pub error: Option<String>,
    pub created: u64,
    pub updated: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    pub snapshot: Option<String>,
    #[serde(default)]
    pub keys: Vec<String>,
    pub prefix: Option<String>,
    #[serde(default)]
    pub selector: Selector,
    #[serde(default)]
    pub archive: bool,
}
#[derive(Default, Deserialize)]
pub struct ListQuery {
    pub snapshot: Option<String>,
    pub prefix: Option<String>,
    pub resource_type: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn id() -> String {
    Uuid::new_v4().to_string()
}
fn export_id(
    snapshot: &str,
    key: &str,
    config: &Config,
    selector: &Selector,
    archive: bool,
    media_identity: &str,
) -> String {
    crypto::digest(
        &serde_json::to_vec(&(
            snapshot,
            key,
            worker::PROFILE,
            media_identity,
            selector,
            archive,
            config.decryption(key),
            crypto::digest(&config.cri_key.to_le_bytes()),
            config.ffmpeg_threads,
            config.local_source.as_ref().map(|s| s.identity()),
        ))
        .unwrap(),
    )
}
fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

struct Budget {
    used: AtomicU64,
    max: u64,
}
struct Reservation {
    budget: Arc<Budget>,
    bytes: u64,
}
impl Budget {
    fn reserve(self: &Arc<Self>, n: u64) -> Result<Reservation> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(n).filter(|v| *v <= self.max)
            })
            .map_err(|_| anyhow::anyhow!("temporary storage budget exhausted"))?;
        Ok(Reservation {
            budget: self.clone(),
            bytes: n,
        })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct Download {
    input: Input,
    raw_sha256: String,
    _dir: tempfile::TempDir,
    _reservation: Reservation,
}

// Own an unindexed publication until its database transaction has committed.
struct PendingPublication(Option<std::path::PathBuf>);
impl Drop for PendingPublication {
    fn drop(&mut self) {
        if let Some(path) = self.0.take()
            && let Err(error) = std::fs::remove_dir_all(&path)
        {
            tracing::error!(%error,"unindexed publication cleanup failed");
        }
    }
}

pub struct App {
    pub config: Config,
    pub db: SqlitePool,
    client: reqwest::Client,
    downloads: Arc<Semaphore>,
    workers: Arc<Semaphore>,
    videos: Arc<Semaphore>,
    queue: Arc<Semaphore>,
    download_registry: Registry<Download>,
    export_registry: Registry<Manifest>,
    refresh_lock: tokio::sync::Mutex<()>,
    resource_locks: Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    catalog_cache: tokio::sync::Mutex<HashMap<String, (Snapshot, Arc<Catalog>)>>,
    cancellations: Mutex<HashMap<String, CancellationToken>>,
    unpersisted_tasks: Mutex<HashMap<String, Task>>,
    budget: Arc<Budget>,
    shutdown: CancellationToken,
    media_identity: std::sync::OnceLock<String>,
    _lock: std::fs::File,
}

impl App {
    pub async fn open(mut config: Config) -> Result<Arc<Self>> {
        config.validate()?;
        std::fs::create_dir_all(&config.data_dir)?;
        config.data_dir = std::fs::canonicalize(&config.data_dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(config.data_dir.join("instance.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("data directory already in use")?;
        for name in ["tmp", "exports", "catalogs"] {
            let p = config.data_dir.join(name);
            ensure!(!p.is_symlink(), "symlink storage directory");
            std::fs::create_dir_all(&p)?;
        }
        for entry in std::fs::read_dir(config.data_dir.join("tmp"))? {
            let p = entry?.path();
            if p.is_dir() && !p.is_symlink() {
                std::fs::remove_dir_all(p)?
            } else {
                std::fs::remove_file(p)?
            }
        }
        let options = SqliteConnectOptions::new()
            .filename(config.data_dir.join("index.sqlite"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(10));
        let db = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&db)
            .await?;
        ensure!(version <= 1, "database schema is newer than this service");
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS snapshots(id TEXT PRIMARY KEY, body TEXT NOT NULL, catalog TEXT NOT NULL, current INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY, body TEXT NOT NULL); CREATE TABLE IF NOT EXISTS exports(id TEXT PRIMARY KEY, body TEXT NOT NULL); CREATE TABLE IF NOT EXISTS files(id TEXT PRIMARY KEY, export_id TEXT NOT NULL, name TEXT NOT NULL, mime TEXT NOT NULL, hash TEXT NOT NULL); PRAGMA user_version=1;").execute(&db).await?;
        let mut previous_id = String::new();
        loop {
            let rows = sqlx::query("SELECT id,body FROM tasks WHERE id>? ORDER BY id LIMIT 100")
                .bind(&previous_id)
                .fetch_all(&db)
                .await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                previous_id = row.get("id");
                let mut t: Task = serde_json::from_str(row.get("body"))?;
                if matches!(t.state.as_str(), "queued" | "running") {
                    t.state = "failed".into();
                    t.error = Some("interrupted by service restart; resubmit failed keys".into());
                    let reported: std::collections::HashSet<_> =
                        t.results.iter().map(|r| r.key.clone()).collect();
                    t.results
                        .extend(t.keys.iter().filter(|k| !reported.contains(*k)).map(|k| {
                            task_runner::failed_item(k.clone(), "interrupted by service restart")
                        }));
                    if t.kind == "export" {
                        t.completed = t.results.len();
                    }
                    t.updated = now();
                    sqlx::query("UPDATE tasks SET body=? WHERE id=?")
                        .bind(serde_json::to_string(&t)?)
                        .bind(&t.id)
                        .execute(&db)
                        .await?;
                }
            }
        }
        // A crash between filesystem publication and SQL commit leaves only an unreferenced directory.
        for entry in std::fs::read_dir(config.data_dir.join("exports"))? {
            let e = entry?;
            let name = e.file_name().to_string_lossy().into_owned();
            ensure!(
                valid_id(&name) && !e.path().is_symlink(),
                "unexpected export storage entry"
            );
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM exports WHERE id=?)")
                    .bind(name)
                    .fetch_one(&db)
                    .await?;
            if !exists {
                std::fs::remove_dir_all(e.path())?;
            }
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(config.download_timeout_secs))
            .build()?;
        let app = Arc::new(Self {
            downloads: Arc::new(Semaphore::new(config.downloads)),
            workers: Arc::new(Semaphore::new(config.workers)),
            videos: Arc::new(Semaphore::new(config.videos)),
            queue: Arc::new(Semaphore::new(config.queue_limit)),
            budget: Arc::new(Budget {
                used: AtomicU64::new(0),
                max: config.temp_bytes,
            }),
            config,
            db,
            client,
            download_registry: Registry::default(),
            export_registry: Registry::default(),
            refresh_lock: tokio::sync::Mutex::new(()),
            resource_locks: Mutex::new(HashMap::new()),
            catalog_cache: tokio::sync::Mutex::new(HashMap::new()),
            cancellations: Mutex::new(HashMap::new()),
            unpersisted_tasks: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            media_identity: std::sync::OnceLock::new(),
            _lock: lock,
        });
        app.media_ready().await?;
        Ok(app)
    }
    async fn media_ready(&self) -> Result<()> {
        let mut versions = serde_json::Map::new();
        for (tool, binary) in [
            ("ffmpeg", &self.config.ffmpeg),
            ("ffprobe", &self.config.ffprobe),
        ] {
            let version =
                diagnostics::inspect_tool(&self.config, binary, &["-version"], tool).await?;
            versions.insert(
                tool.into(),
                json!(
                    String::from_utf8_lossy(&version)
                        .lines()
                        .next()
                        .unwrap_or("unknown")
                        .chars()
                        .take(1024)
                        .collect::<String>()
                ),
            );
        }
        self.media_identity
            .set(crypto::digest(&serde_json::to_vec(&versions)?))
            .map_err(|_| anyhow::anyhow!("media versions already initialized"))?;
        diagnostics::save_versions(&self.config, &Value::Object(versions))?;
        let codecs = diagnostics::inspect_tool(
            &self.config,
            &self.config.ffmpeg,
            &["-hide_banner", "-encoders"],
            "ffmpeg",
        )
        .await?;
        let list = String::from_utf8_lossy(&codecs);
        ensure!(
            list.contains("libx264")
                && list
                    .lines()
                    .any(|l| l.split_whitespace().nth(1) == Some("ffv1"))
                && list
                    .lines()
                    .any(|l| l.split_whitespace().nth(1) == Some("aac")),
            "FFmpeg AAC/libx264/FFV1 required"
        );
        Ok(())
    }
    pub async fn stop(&self) {
        self.shutdown.cancel();
        for c in self.cancellations.lock().unwrap().values() {
            c.cancel();
        }
        let _ = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if self.cancellations.lock().unwrap().is_empty()
                    && self.budget.used.load(Ordering::Acquire) == 0
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
    }
    async fn save_task(&self, t: &Task) -> Result<()> {
        sqlx::query("INSERT INTO tasks(id,body) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET body=excluded.body").bind(&t.id).bind(serde_json::to_string(t)?).execute(&self.db).await?;
        Ok(())
    }
    async fn finish_task(&self, task: &mut Task) {
        if let Err(error) = self.save_task(task).await {
            tracing::error!(task=%task.id, %error, "task final persistence failed");
            task_runner::persistence_failed(task);
            // One bounded retry persists the failure itself when storage recovers.
            if let Err(error) = self.save_task(task).await {
                tracing::error!(task=%task.id, %error, "task failure retained in memory; storage repair required");
                self.unpersisted_tasks
                    .lock()
                    .unwrap()
                    .insert(task.id.clone(), task.clone());
            }
        }
    }
    async fn new_task(
        &self,
        kind: &str,
        snapshot: Option<String>,
        total: usize,
        keys: Vec<String>,
    ) -> Result<(Task, CancellationToken, OwnedSemaphorePermit)> {
        ensure!(!self.shutdown.is_cancelled(), "service shutting down");
        ensure!(
            self.unpersisted_tasks.lock().unwrap().is_empty(),
            "task persistence unavailable; restart after repairing storage"
        );
        let permit = self
            .queue
            .clone()
            .try_acquire_owned()
            .context("task queue full")?;
        let t = Task {
            id: id(),
            kind: kind.into(),
            state: "queued".into(),
            snapshot,
            total,
            keys,
            completed: 0,
            results: vec![],
            error: None,
            created: now(),
            updated: now(),
        };
        self.save_task(&t).await?;
        let token = self.shutdown.child_token();
        self.cancellations
            .lock()
            .unwrap()
            .insert(t.id.clone(), token.clone());
        Ok((t, token, permit))
    }
    pub async fn snapshot(&self, id: Option<&str>) -> Result<(Snapshot, Arc<Catalog>)> {
        let sid = if let Some(id) = id {
            id.to_string()
        } else {
            sqlx::query_scalar::<_, String>("SELECT id FROM snapshots WHERE current=1")
                .fetch_optional(&self.db)
                .await?
                .context("catalog snapshot not found; refresh first")?
        };
        let mut cache = self.catalog_cache.lock().await;
        if let Some(value) = cache.get(&sid) {
            return Ok(value.clone());
        }
        cache.retain(|_, (_, c)| Arc::strong_count(c) > 1);
        ensure!(cache.len() < 8, "too many active catalog snapshots");
        let id = Some(sid.as_str());
        let row = if let Some(id) = id {
            sqlx::query("SELECT body,catalog FROM snapshots WHERE id=?")
                .bind(id)
                .fetch_optional(&self.db)
                .await?
        } else {
            sqlx::query("SELECT body,catalog FROM snapshots WHERE current=1")
                .fetch_optional(&self.db)
                .await?
        }
        .context("catalog snapshot not found; refresh first")?;
        let value = (
            serde_json::from_str(row.get("body"))?,
            Arc::new(serde_json::from_str(row.get("catalog"))?),
        );
        cache.insert(sid, value.clone());
        Ok(value)
    }
    async fn fetch_bytes(
        &self,
        url: url::Url,
        max: usize,
        token: &CancellationToken,
    ) -> Result<Vec<u8>> {
        let response = tokio::select! {_ = token.cancelled()=>bail!("cancelled"),r=self.client.get(url).send()=>r?};
        ensure!(
            response.status() == StatusCode::OK,
            "CDN HTTP {}",
            response.status()
        );
        ensure!(
            response.content_length().is_none_or(|n| n <= max as u64),
            "response size limit"
        );
        let mut stream = response.bytes_stream();
        let mut bytes = vec![];
        loop {
            let chunk =
                tokio::select! {_ = token.cancelled()=>bail!("cancelled"),c=stream.try_next()=>c?};
            let Some(chunk) = chunk else { break };
            ensure!(
                chunk.len() <= max.saturating_sub(bytes.len()),
                "response size limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    pub async fn submit_refresh(self: &Arc<Self>) -> Result<Task> {
        let (mut task, token, permit) = self.new_task("catalog_refresh", None, 1, vec![]).await?;
        let answer = task.clone();
        let app = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            task.state = "running".into();
            if let Err(error) = app.save_task(&task).await {
                tracing::error!(task=%task.id, %error, "task start persistence failed");
                task_runner::persistence_failed(&mut task);
                app.finish_task(&mut task).await;
                app.cancellations.lock().unwrap().remove(&task.id);
                return;
            }
            let result = app.refresh(&token).await;
            match result {
                Ok(s) => {
                    task.snapshot = Some(s);
                    task.completed = 1;
                    task.state = "succeeded".into();
                }
                Err(e) => {
                    task.state = if token.is_cancelled() {
                        "cancelled"
                    } else {
                        "failed"
                    }
                    .into();
                    task.error = Some(e.to_string());
                }
            }
            task.updated = now();
            app.finish_task(&mut task).await;
            app.cancellations.lock().unwrap().remove(&task.id);
        });
        Ok(answer)
    }
    async fn refresh(&self, token: &CancellationToken) -> Result<String> {
        let _guard = tokio::select! {_ = token.cancelled()=>bail!("cancelled"),g=self.refresh_lock.lock()=>g};
        let hash = self
            .fetch_bytes(self.config.catalog_url("hash")?, 65536, token)
            .await?;
        let remote_hash = std::str::from_utf8(&hash)?.trim().to_string();
        ensure!(remote_hash.len() <= 128, "invalid remote hash");
        let bytes = self
            .fetch_bytes(self.config.catalog_url("bin")?, 32 << 20, token)
            .await?;
        let catalog = Catalog::parse(&bytes)?;
        let digest = crypto::digest(&bytes);
        let sid = crypto::digest(&serde_json::to_vec(&(
            &self.config.region,
            &self.config.locale,
            &self.config.bili_version,
            &self.config.cdn_root,
            &digest,
        ))?);
        let s = Snapshot {
            id: sid.clone(),
            content_sha256: digest,
            region: self.config.region.clone(),
            locale: self.config.locale.clone(),
            bili_version: self.config.bili_version.clone(),
            cdn_root: self.config.cdn_root.clone(),
            remote_hash,
            created: now(),
        };
        ensure!(!token.is_cancelled(), "cancelled");
        let mut tmp = tempfile::NamedTempFile::new_in(self.config.data_dir.join("catalogs"))?;
        std::io::Write::write_all(&mut tmp, &bytes)?;
        tmp.as_file().sync_all()?;
        let target = self
            .config
            .data_dir
            .join("catalogs")
            .join(format!("{sid}.bin"));
        if !target.exists() {
            tmp.persist_noclobber(target)?;
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE snapshots SET current=0")
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO snapshots(id,body,catalog,current) VALUES(?,?,?,1) ON CONFLICT(id) DO UPDATE SET current=1").bind(&sid).bind(serde_json::to_string(&s)?).bind(serde_json::to_string(&catalog)?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(sid)
    }
    pub async fn manifest(&self, id: &str) -> Result<Option<Manifest>> {
        let body: Option<String> = sqlx::query_scalar("SELECT body FROM exports WHERE id=?")
            .bind(id)
            .fetch_optional(&self.db)
            .await?;
        body.map(|b| serde_json::from_str(&b).map_err(Into::into))
            .transpose()
    }
    pub async fn submit_export(self: &Arc<Self>, request: ExportRequest) -> Result<Task> {
        ensure!(
            request.prefix.is_some() != !request.keys.is_empty(),
            "provide either keys or prefix"
        );
        let (snapshot, catalog) = self.snapshot(request.snapshot.as_deref()).await?;
        let mut keys = if let Some(prefix) = request.prefix {
            catalog
                .keys
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .take(self.config.max_keys + 1)
                .cloned()
                .collect()
        } else {
            request.keys
        };
        keys.sort();
        keys.dedup();
        ensure!(
            !keys.is_empty() && keys.len() <= self.config.max_keys,
            "empty or excessive selection"
        );
        ensure!(keys.iter().all(|k| k.len() <= 4096), "key limit");
        let selector = request.selector;
        ensure!(
            selector.location_id.is_none() || keys.len() == 1,
            "location_id requires exactly one key"
        );
        let archive = request.archive;
        let (mut task, token, permit) = self
            .new_task(
                "export",
                Some(snapshot.id.clone()),
                keys.len(),
                keys.clone(),
            )
            .await?;
        let answer = task.clone();
        let app = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            task.state = "running".into();
            if let Err(error) = app.save_task(&task).await {
                tracing::error!(task=%task.id, %error, "task start persistence failed");
                task_runner::persistence_failed(&mut task);
                token.cancel();
            }
            task_runner::run(
                &mut task,
                keys,
                app.config.downloads,
                &token,
                |key| {
                    let app = app.clone();
                    let snapshot = snapshot.clone();
                    let catalog = catalog.clone();
                    let token = token.clone();
                    let selector=selector.clone();
                    async move {
                        let started = Instant::now();
                        let eid = export_id(&snapshot.id, &key, &app.config, &selector, archive, app.media_identity.get().unwrap());
                        let result = app
                            .export_shared(snapshot, catalog, key.clone(), selector, archive, &token)
                            .await;
                        tracing::info!(export=%eid, elapsed_ms=started.elapsed().as_millis(), success=result.is_ok(), "resource finished");
                        match result {
                            Ok(m) => ItemResult {
                                key,
                                export_id: Some(m.id),
                                error: None,
                            },
                            Err(e) => task_runner::failed_item(key, &e.to_string()),
                        }
                    }
                },
                |task| {
                    let app = app.clone();
                    async move { app.save_task(&task).await }
                },
            )
            .await;
            app.finish_task(&mut task).await;
            app.cancellations.lock().unwrap().remove(&task.id);
        });
        Ok(answer)
    }
    async fn export_shared(
        self: &Arc<Self>,
        snapshot: Snapshot,
        catalog: Arc<Catalog>,
        key: String,
        selector: Selector,
        archive: bool,
        token: &CancellationToken,
    ) -> Result<Manifest> {
        let eid = export_id(
            &snapshot.id,
            &key,
            &self.config,
            &selector,
            archive,
            self.media_identity.get().unwrap(),
        );
        if let Some(m) = self.manifest(&eid).await? {
            return Ok(m);
        }
        let app = self.clone();
        let result = shared::join(
            &self.export_registry,
            eid.clone(),
            token,
            move |cancel| async move {
                app.export_one(snapshot, catalog, key, (selector, archive), eid, cancel)
                    .await
            },
        )
        .await?;
        Ok((*result).clone())
    }
    async fn download(
        self: &Arc<Self>,
        snapshot: Snapshot,
        location: Location,
        token: &CancellationToken,
    ) -> Result<shared::Lease<Download>> {
        let key = format!("{}:{}", snapshot.id, location.id);
        let app = self.clone();
        shared::join(&self.download_registry,key,token,move|cancel|async move{
            let _slot=tokio::select!{_ = cancel.cancelled()=>bail!("cancelled"),p=app.downloads.clone().acquire_owned()=>p?};
            let opts=location.options.as_ref().context("missing bundle options")?;ensure!(opts.size>0&&opts.size<=app.config.input_bytes,"input size budget");
            let reservation=app.budget.reserve(opts.size)?;ensure!(fs2::available_space(&app.config.data_dir)?>opts.size,"insufficient free disk space");let dir=tempfile::Builder::new().prefix("download-").tempdir_in(app.config.data_dir.join("tmp"))?;
            let name=location.internal.rsplit('/').next().context("basename")?;ensure!(!name.is_empty(),"empty basename");let path=dir.path().join("payload");
            if !location.internal.starts_with("http://") && !location.internal.starts_with("https://") {
                let source=app.config.local_source.clone().context("local dependency requires configured source")?;
                let internal=location.internal.clone();let target=path.clone();let limit=app.config.input_bytes;
                let sha=tokio::task::spawn_blocking(move ||source.copy_verified(&internal,&target,limit)).await??;
                ensure!(path.metadata()?.len()==opts.size,"local/catalog size mismatch");
                if location.provider==CRYPT&&!crypto::builtin(name){
                    use tokio::io::{AsyncReadExt,AsyncSeekExt};
                    let mut file=tokio::fs::OpenOptions::new().read(true).write(true).open(&path).await?;
                    let mut prefix=vec![0;opts.size.min(16384) as usize];file.read_exact(&mut prefix).await?;
                    crypto::decrypt(&mut prefix,name,0)?;file.seek(std::io::SeekFrom::Start(0)).await?;file.write_all(&prefix).await?;file.sync_all().await?;
                }
                ensure!(!cancel.is_cancelled(),"cancelled");
                return Ok(Download{input:Input{location,path},raw_sha256:sha,_dir:dir,_reservation:reservation});
            }
            let config=Config{cdn_root:snapshot.cdn_root,..app.config.clone()};let url=config.asset_url(&location.internal)?;
            let response=tokio::select!{_ = cancel.cancelled()=>bail!("cancelled"),r=app.client.get(url).send()=>r?};ensure!(response.status()==StatusCode::OK,"CDN HTTP {}",response.status());ensure!(response.content_length().is_none_or(|n|n==opts.size),"Content-Length mismatch");
            use sha2::Digest;let mut hash=sha2::Sha256::new();let mut stream=response.bytes_stream();let mut f=tokio::fs::File::create(&path).await?;let mut received=0u64;
            while let Some(chunk)=tokio::select!{_ = cancel.cancelled()=>bail!("cancelled"),v=stream.try_next()=>v?}{ensure!(received+chunk.len() as u64<=opts.size,"download exceeds catalog size");hash.update(&chunk);let mut bytes=chunk.to_vec();if location.provider==CRYPT&&!crypto::builtin(name){crypto::decrypt(&mut bytes,name,received)?;}f.write_all(&bytes).await?;received+=chunk.len() as u64;}
            ensure!(received==opts.size,"truncated download");f.flush().await?;f.sync_all().await?;drop(f);
            Ok(Download{input:Input{location,path},raw_sha256:hex::encode(hash.finalize()),_dir:dir,_reservation:reservation})
        }).await
    }
    async fn export_one(
        self: Arc<Self>,
        snapshot: Snapshot,
        catalog: Arc<Catalog>,
        key: String,
        selection: (Selector, bool),
        eid: String,
        cancel: CancellationToken,
    ) -> Result<Manifest> {
        let gate = {
            let mut map = self.resource_locks.lock().unwrap();
            map.retain(|_, v| v.strong_count() > 0);
            if let Some(g) = map.get(&eid).and_then(std::sync::Weak::upgrade) {
                g
            } else {
                let g = Arc::new(tokio::sync::Mutex::new(()));
                map.insert(eid.clone(), Arc::downgrade(&g));
                g
            }
        };
        let _gate = tokio::select! {_ = cancel.cancelled()=>bail!("cancelled"),g=gate.lock()=>g};
        if let Some(m) = self.manifest(&eid).await? {
            return Ok(m);
        }
        let (selector, archive) = selection;
        let mut target = catalog.resolve(&key, &selector)?.clone();
        target.key = key.clone();
        let mut closure = catalog.closure_from(target.id)?;
        // Playback wrapper dependencies are not required for the unique raw CRI payload.
        if target.resource_type.starts_with("CriWare.") && !archive {
            closure.retain(|l| l.provider == crate::catalog::CRI);
            ensure!(closure.len() <= 1, "ambiguous CRI media dependencies");
            if closure.is_empty() {
                closure = catalog.closure_from(target.id)?;
                // Serialized embedded CRI implementations carry their bytes in
                // the target bundle's type tree; MonoScript playback assemblies
                // are not used to read that byte array.
                closure.retain(|l| {
                    !l.internal
                        .rsplit('/')
                        .next()
                        .is_some_and(|n| n.to_ascii_lowercase().contains("monoscripts"))
                });
            }
        }
        ensure!(closure.len() <= 512, "dependency count limit");
        // Retain shared payload references until this resource finishes.
        let mut inputs = futures_util::stream::iter(closure)
            .map(|l| self.download(snapshot.clone(), l, &cancel))
            .buffer_unordered(self.config.downloads)
            .try_collect::<Vec<_>>()
            .await?;
        inputs.sort_by_key(|v| v.input.location.id);
        let mut video_input = false;
        for input in &inputs {
            if input.input.location.provider == crate::catalog::CRI {
                use tokio::io::AsyncReadExt;
                let mut file = tokio::fs::File::open(&input.input.path).await?;
                let mut magic = [0; 4];
                file.read_exact(&mut magic).await?;
                video_input |= &magic == b"CRID";
            }
        }
        let _video = if video_input {
            Some(
                tokio::select! {_ = cancel.cancelled()=>bail!("cancelled"),p=self.videos.clone().acquire_owned()=>p?},
            )
        } else {
            None
        };
        let _worker = tokio::select! {_ = cancel.cancelled()=>bail!("cancelled"),p=self.workers.clone().acquire_owned()=>p?};
        let _storage = self
            .budget
            .reserve(self.config.output_bytes + self.config.expanded_bytes * 2)?;
        ensure!(
            fs2::available_space(&self.config.data_dir)?
                >= self.config.output_bytes + self.config.expanded_bytes * 2,
            "insufficient free disk space"
        );
        let dir = tempfile::Builder::new()
            .prefix("worker-")
            .tempdir_in(self.config.data_dir.join("tmp"))?;
        let output = dir.path().join("output");
        let job_path = dir.path().join("job.json");
        let job = Job {
            config: self.config.clone(),
            target,
            inputs: inputs.iter().map(|v| v.input.clone()).collect(),
            output: output.clone(),
            archive,
        };
        tokio::fs::write(&job_path, serde_json::to_vec(&job)?).await?;
        let exe = std::env::current_exe()?;
        let (cpu_seconds, wall_seconds) = self.config.worker_limits(video_input);
        let mut cmd = Command::new("prlimit");
        cmd.args([
            format!("--as={}", self.config.worker_memory_bytes),
            format!(
                "--fsize={}",
                self.config.output_bytes.max(self.config.expanded_bytes)
            ),
            format!("--cpu={}:{}", cpu_seconds, cpu_seconds.saturating_add(1)),
            "--nofile=256".into(),
            "--".into(),
        ])
        .arg(exe)
        .arg("worker")
        .arg(&job_path)
        .env(
            "MOENOTES_DIAGNOSTIC_CONTEXT",
            serde_json::to_string(&json!({
                "export_id":eid,"snapshot":snapshot.id,"key":key,
                "input_sha256":inputs.iter().map(|i| &i.raw_sha256).collect::<Vec<_>>()
            }))?,
        )
        .kill_on_drop(true)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|_| {
            diagnostics::failure(
                &self.config,
                "worker_spawn_failed",
                "worker",
                "worker",
                None,
                started,
                &Default::default(),
            )
        })?;
        let pid = child.id().context("worker PID")?;
        let stderr = child.stderr.take().context("worker stderr")?;
        let drain = tokio::spawn(diagnostics::drain_stderr(stderr));
        let outcome = tokio::select! {
            _ = cancel.cancelled() => Err("worker_cancelled"),
            r = tokio::time::timeout(Duration::from_secs(wall_seconds), child.wait()) => match r {
                Ok(Ok(status)) => Ok(status), Ok(Err(_)) => Err("worker_wait_failed"), Err(_) => Err("worker_timeout"),
            }
        };
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let reaped = child.wait().await.ok();
        let stderr = drain.await.context("worker stderr task")??;
        let status = match outcome {
            Ok(status) => status,
            Err(code) => {
                return Err(diagnostics::failure(
                    &self.config,
                    code,
                    "worker",
                    "worker",
                    reaped,
                    started,
                    &stderr,
                )
                .into());
            }
        };
        if !status.success() {
            use std::os::unix::process::ExitStatusExt;
            let code = match status.signal() {
                Some(nix::libc::SIGXCPU) => "worker_cpu_limit",
                Some(_) => "worker_signal",
                None => "worker_exit_failed",
            };
            return Err(diagnostics::failure(
                &self.config,
                code,
                "worker",
                "worker",
                Some(status),
                started,
                &stderr,
            )
            .into());
        }
        ensure!(!cancel.is_cancelled(), "cancelled");
        let result_path = job_path.with_extension("result.json");
        ensure!(
            result_path.metadata()?.len() <= 16 << 20,
            "worker result limit"
        );
        let r: worker::WorkerResult = serde_json::from_slice(&tokio::fs::read(result_path).await?)?;
        if let Some(e) = r.error {
            bail!("{e}")
        };
        ensure!(!r.files.is_empty() || r.empty, "empty worker result");
        let mut files = vec![];
        let mut total = 0;
        for a in r.files {
            ensure!(
                a.name.len() < 100
                    && std::path::Path::new(&a.name)
                        .file_name()
                        .is_some_and(|n| n == a.name.as_str()),
                "unsafe worker output name"
            );
            let p = output.join(&a.name);
            ensure!(
                !p.is_symlink() && p.is_file() && p.metadata()?.len() == a.bytes,
                "invalid output file"
            );
            total += a.bytes;
            ensure!(total <= self.config.output_bytes, "output budget");
            FileSync::sync(&p)?;
            files.push(PublishedFile {
                id: crypto::digest(format!("{eid}:{}", a.name).as_bytes()),
                artifact: a,
            });
        }
        let manifest = Manifest {
            id: eid.clone(),
            snapshot: snapshot.id,
            key,
            profile: worker::PROFILE.into(),
            files,
            selected_location: Some(job.target.id),
            empty: r.empty,
            options:json!({"selector":selector,"archive":archive,"decryption":self.config.decryption(&job.target.key),"ffmpeg_threads":self.config.ffmpeg_threads,"media_identity":self.media_identity.get(),"local_source_identity":self.config.local_source.as_ref().map(|s|s.identity())}),
            sources:inputs.iter().map(|i|json!({"internal_id":i.input.location.internal,"provider":i.input.location.provider,"options":i.input.location.options,"download_sha256":i.raw_sha256})).collect(),
        };
        tokio::fs::write(
            output.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )
        .await?;
        FileSync::sync(&output.join("manifest.json"))?;
        FileSync::sync(&output)?;
        let destination = self.config.data_dir.join("exports").join(&eid);
        tokio::fs::rename(&output, &destination).await?;
        let mut publication = PendingPublication(Some(destination));
        FileSync::sync(&self.config.data_dir.join("exports"))?;
        let commit = async {
            let mut tx = self.db.begin().await?;
            sqlx::query("INSERT INTO exports(id,body) VALUES(?,?)")
                .bind(&eid)
                .bind(serde_json::to_string(&manifest)?)
                .execute(&mut *tx)
                .await?;
            for f in &manifest.files {
                sqlx::query("INSERT INTO files(id,export_id,name,mime,hash) VALUES(?,?,?,?,?)")
                    .bind(&f.id)
                    .bind(&eid)
                    .bind(&f.artifact.name)
                    .bind(&f.artifact.media_type)
                    .bind(&f.artifact.sha256)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        commit?;
        publication.0 = None;
        tracing::info!(export=%eid,files=manifest.files.len(),"resource published");
        Ok(manifest)
    }
}
struct FileSync;
impl FileSync {
    fn sync(p: &std::path::Path) -> Result<()> {
        std::fs::File::open(p)?.sync_all()?;
        Ok(())
    }
}

pub struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = self.0.to_string();
        let status = if message.contains("task persistence unavailable") {
            StatusCode::SERVICE_UNAVAILABLE
        } else if message.contains("not found") {
            StatusCode::NOT_FOUND
        } else if message.contains("queue full") {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, Json(json!({"error":message}))).into_response()
    }
}
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/readyz", get(ready))
        .route("/v1/catalogs", get(catalogs))
        .route("/v1/catalogs/refresh", post(refresh))
        .route("/v1/assets", get(assets))
        .route("/v1/exports", post(exports))
        .route("/v1/preflight", post(preflight))
        .route("/v1/exports/{id}", get(manifest))
        .route("/v1/tasks/{id}", get(task))
        .route("/v1/tasks/{id}/cancel", post(cancel))
        .route("/v1/files/{id}", get(file))
        .layer(DefaultBodyLimit::max(2 << 20))
        .with_state(app)
}
async fn ready(State(app): State<Arc<App>>) -> std::result::Result<Json<Value>, ApiError> {
    if !app.unpersisted_tasks.lock().unwrap().is_empty() {
        return Err(anyhow::anyhow!("task persistence unavailable").into());
    }
    sqlx::query("SELECT 1").execute(&app.db).await?;
    if app.shutdown.is_cancelled() {
        return Err(anyhow::anyhow!("shutting down").into());
    }
    let p = tempfile::NamedTempFile::new_in(app.config.data_dir.join("tmp"))?;
    p.as_file().sync_all()?;
    Ok(Json(
        json!({"status":"ready","temporary_reserved_bytes":app.budget.used.load(Ordering::Acquire)}),
    ))
}
async fn refresh(State(app): State<Arc<App>>) -> std::result::Result<impl IntoResponse, ApiError> {
    Ok((StatusCode::ACCEPTED, Json(app.submit_refresh().await?)))
}
async fn catalogs(State(app): State<Arc<App>>) -> std::result::Result<Json<Value>, ApiError> {
    let mut out = vec![];
    for r in sqlx::query("SELECT body,current FROM snapshots ORDER BY rowid DESC")
        .fetch_all(&app.db)
        .await?
    {
        out.push(json!({"snapshot":serde_json::from_str::<Value>(r.get("body"))?,"current":r.get::<i64,_>("current")==1}));
    }
    Ok(Json(json!({"snapshots":out})))
}
async fn assets(
    State(app): State<Arc<App>>,
    Query(q): Query<ListQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    let (s, c) = app.snapshot(q.snapshot.as_deref()).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0);
    let rows: Vec<_> = c
        .keys
        .keys()
        .filter(|k| q.prefix.as_ref().is_none_or(|p| k.starts_with(p)))
        .filter_map(|k| c.target(k).ok().map(|l| (k, l)))
        .filter(|(_, l)| {
            q.resource_type
                .as_ref()
                .is_none_or(|t| l.resource_type == *t)
        })
        .skip(offset)
        .take(limit)
        .map(|(k, l)| json!({"key":k,"resource_type":l.resource_type,"provider":l.provider}))
        .collect();
    Ok(Json(json!({"snapshot":s.id,"offset":offset,"items":rows})))
}
async fn exports(
    State(app): State<Arc<App>>,
    Json(r): Json<ExportRequest>,
) -> std::result::Result<impl IntoResponse, ApiError> {
    Ok((StatusCode::ACCEPTED, Json(app.submit_export(r).await?)))
}
async fn task(
    State(app): State<Arc<App>>,
    Param(id): Param<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    if let Some(task) = app.unpersisted_tasks.lock().unwrap().get(&id) {
        return Ok(Json(serde_json::to_value(task)?));
    }
    let b: Option<String> = sqlx::query_scalar("SELECT body FROM tasks WHERE id=?")
        .bind(id)
        .fetch_optional(&app.db)
        .await?;
    Ok(Json(serde_json::from_str(&b.context("task not found")?)?))
}
async fn cancel(
    State(app): State<Arc<App>>,
    Param(id): Param<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?)")
        .bind(&id)
        .fetch_one(&app.db)
        .await?;
    if !exists {
        return Err(anyhow::anyhow!("task not found").into());
    }
    if let Some(c) = app.cancellations.lock().unwrap().get(&id) {
        c.cancel();
    }
    Ok(Json(json!({"id":id,"cancellation_requested":true})))
}
async fn manifest(
    State(app): State<Arc<App>>,
    Param(id): Param<String>,
) -> std::result::Result<Json<Manifest>, ApiError> {
    Ok(Json(app.manifest(&id).await?.context("export not found")?))
}
async fn file(
    State(app): State<Arc<App>>,
    Param(id): Param<String>,
    mut request: Request,
) -> std::result::Result<Response, ApiError> {
    let r = sqlx::query("SELECT export_id,name,mime,hash FROM files WHERE id=?")
        .bind(&id)
        .fetch_optional(&app.db)
        .await?
        .context("file not found")?;
    let export: String = r.get("export_id");
    let name: String = r.get("name");
    let mime: String = r.get("mime");
    let hash: String = r.get("hash");
    let path = app.config.data_dir.join("exports").join(export).join(name);
    let metadata = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|_| anyhow::anyhow!("file not found"))?;
    if !metadata.is_file() {
        return Err(anyhow::anyhow!("file not found").into());
    }
    let tag = format!("\"{hash}\"");
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().strip_prefix("W/").unwrap_or(t.trim()) == tag || t.trim() == "*")
        })
    {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, tag),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=31536000, immutable".into(),
                ),
            ],
        )
            .into_response());
    }
    if request
        .headers()
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v != tag)
    {
        request.headers_mut().remove(header::RANGE);
    }
    let response = ServeFile::new(path)
        .oneshot(request)
        .await
        .map_err(|e| anyhow::anyhow!("file IO: {e}"))?;
    let mut response = response.map(axum::body::Body::new);
    if response.status().is_success() {
        response.headers_mut().insert(header::ETAG, tag.parse()?);
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, mime.parse()?);
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            "public, max-age=31536000, immutable".parse()?,
        );
        response
            .headers_mut()
            .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse()?);
    } else {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse()?);
    }
    Ok(response)
}

async fn preflight(
    State(app): State<Arc<App>>,
    Json(request): Json<ExportRequest>,
) -> std::result::Result<Json<Value>, ApiError> {
    ensure_preflight(&app, request)
        .await
        .map(Json)
        .map_err(Into::into)
}
async fn ensure_preflight(app: &App, request: ExportRequest) -> Result<Value> {
    ensure!(
        request.prefix.is_some() != !request.keys.is_empty(),
        "provide either keys or prefix"
    );
    let (snapshot, catalog) = app.snapshot(request.snapshot.as_deref()).await?;
    let mut keys: Vec<_> = if let Some(prefix) = request.prefix {
        catalog
            .keys
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .take(app.config.max_keys + 1)
            .cloned()
            .collect()
    } else {
        request.keys
    };
    keys.sort();
    keys.dedup();
    ensure!(
        !keys.is_empty() && keys.len() <= app.config.max_keys,
        "empty or excessive selection"
    );
    ensure!(
        request.selector.location_id.is_none() || keys.len() == 1,
        "location_id requires exactly one key"
    );
    let mut results = vec![];
    for key in keys {
        let candidates: Vec<_> = catalog
            .keys
            .get(&key)
            .into_iter()
            .flatten()
            .filter_map(|id| catalog.locations.get(id))
            .map(|l| json!({"location_id":l.id,"resource_type":l.resource_type}))
            .collect();
        let target = match catalog.resolve(&key, &request.selector) {
            Ok(v) => v,
            Err(e) => {
                results.push(json!({"key":key,"status":if candidates.len()>1{"ambiguous"}else{"missing"},"error":e.to_string(),"candidates":candidates}));
                continue;
            }
        };
        let mut closure = match catalog.closure_from(target.id) {
            Ok(v) => v,
            Err(e) => {
                results.push(json!({"key":key,"status":"missing","error":e.to_string()}));
                continue;
            }
        };
        let mut embedded_cri = false;
        if target.resource_type.starts_with("CriWare.") && !request.archive {
            let raw: Vec<_> = closure
                .iter()
                .filter(|l| l.provider == crate::catalog::CRI)
                .cloned()
                .collect();
            if raw.is_empty() {
                embedded_cri = true;
                closure.retain(|l| {
                    !l.internal
                        .rsplit('/')
                        .next()
                        .is_some_and(|n| n.to_ascii_lowercase().contains("monoscripts"))
                });
            } else {
                closure = raw;
            }
        }
        let mut dependencies = vec![];
        let mut missing = false;
        let mut local = false;
        for l in &closure {
            let remote = l.internal.starts_with("https://") || l.internal.starts_with("http://");
            let status = if remote {
                "remote"
            } else {
                local = true;
                if app.config.local_source.as_ref().is_some_and(|s| {
                    s.open(&l.internal)
                        .is_ok_and(|(_, e)| l.options.as_ref().is_some_and(|o| o.size == e.bytes))
                }) {
                    "local"
                } else {
                    missing = true;
                    "missing"
                }
            };
            dependencies.push(json!({"location_id":l.id,"internal_id":l.internal,"status":status}));
        }
        let supported = request.archive
            || matches!(
                target.resource_type.as_str(),
                "UnityEngine.Texture2D"
                    | "UnityEngine.Sprite"
                    | "UnityEngine.TextAsset"
                    | "UnityEngine.U2D.SpriteAtlas"
            )
            || target.resource_type.starts_with("CriWare.")
            || target.provider == crate::catalog::CRI;
        let cri = target.resource_type.starts_with("CriWare.") && !request.archive;
        let status = if cri && !embedded_cri && closure.len() > 1 {
            "ambiguous"
        } else if missing || closure.is_empty() {
            "missing"
        } else if !supported {
            "unsupported"
        } else if local {
            "local"
        } else {
            "remote"
        };
        results.push(json!({"key":key,"status":status,"location_id":target.id,"resource_type":target.resource_type,"dependencies":dependencies,"candidates":candidates,"payload":if embedded_cri{"embedded_cri_candidate"}else{"raw"},"error":if cri&&closure.is_empty(){Some("missing_cri_payload")}else if cri&&!embedded_cri&&closure.len()>1{Some("ambiguous_cri_payload")}else{None}}));
    }
    Ok(
        json!({"snapshot":snapshot.id,"profile":worker::PROFILE,"validation":"dependency availability only; payload hashes and codec validity are checked during export","results":results}),
    )
}

#[cfg(test)]
mod review_tests {
    use super::*;
    #[tokio::test]
    async fn final_sql_failure_is_visible_and_blocks_new_work() {
        use http_body_util::BodyExt;
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Config {
            data_dir: dir.path().into(),
            cdn_root: "https://cdn.invalid".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let (mut task, _, permit) = app
            .new_task("export", None, 1, vec!["one".into()])
            .await
            .unwrap();
        task.results.push(ItemResult {
            key: "one".into(),
            export_id: Some("published".into()),
            error: None,
        });
        task.completed = 1;
        task.state = "succeeded".into();
        sqlx::raw_sql("CREATE TRIGGER fail_final BEFORE UPDATE ON tasks WHEN json_extract(NEW.body, '$.state') IN ('succeeded','failed') BEGIN SELECT RAISE(FAIL, 'injected write failure'); END;").execute(&app.db).await.unwrap();
        app.finish_task(&mut task).await;
        drop(permit);
        assert_eq!(task.state, "failed");
        assert!(task.error.as_deref().unwrap().contains("persistence"));
        let response = router(app.clone())
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/tasks/{}", task.id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let task: Task = serde_json::from_slice(&body).unwrap();
        assert_eq!(task.state, "failed");
        assert_eq!(task.results[0].export_id.as_deref(), Some("published"));
        assert!(
            app.new_task("export", None, 1, vec!["two".into()])
                .await
                .is_err()
        );
        let response = router(app.clone())
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        app.cancellations.lock().unwrap().clear();
        app.stop().await;
    }
    #[tokio::test]
    async fn tree_filters_old_profile_and_rejects_case_collisions() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let app = App::open(Config {
            data_dir: data.clone(),
            cdn_root: "https://cdn.invalid".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        for (id, profile, key) in [
            ("a", "json-png-aac-h264-v1", "Image/A"),
            ("b", worker::PROFILE, "Image/A"),
        ] {
            let path = data.join("exports").join(id);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("00000.txt"), b"fixture").unwrap();
            let m = Manifest {
                id: id.into(),
                snapshot: "s".into(),
                key: key.into(),
                profile: profile.into(),
                files: vec![PublishedFile {
                    id: format!("f{id}"),
                    artifact: Artifact {
                        name: "00000.txt".into(),
                        label: "same".into(),
                        media_type: "text/plain".into(),
                        bytes: 7,
                        sha256: crypto::digest(b"fixture"),
                        metadata: json!({"stable_id":"object-1"}),
                    },
                }],
                sources: vec![],
                selected_location: None,
                empty: false,
                options: Value::Null,
            };
            sqlx::query("INSERT INTO exports(id,body) VALUES(?,?)")
                .bind(id)
                .bind(serde_json::to_string(&m).unwrap())
                .execute(&app.db)
                .await
                .unwrap();
        }
        let tree = root.path().join("tree");
        let index = crate::tree::export(&data, &tree, None, false)
            .await
            .unwrap();
        assert_eq!(index.objects.len(), 1);
        assert_eq!(index.objects[0].profile, worker::PROFILE);
        let old = crate::tree::export_profile(
            &data,
            &root.path().join("old"),
            None,
            true,
            "json-png-aac-h264-v1",
        )
        .await
        .unwrap();
        assert_eq!(old.objects[0].export_id, "a");
        let mut m = app.manifest("b").await.unwrap().unwrap();
        m.id = "c".into();
        m.key = "Image/a".into();
        m.files[0].id = "other".into();
        m.files[0].artifact.metadata = json!({"stable_id":"other"});
        std::fs::create_dir(data.join("exports/c")).unwrap();
        std::fs::write(data.join("exports/c/00000.txt"), b"fixture").unwrap();
        sqlx::query("INSERT INTO exports(id,body) VALUES(?,?)")
            .bind("c")
            .bind(serde_json::to_string(&m).unwrap())
            .execute(&app.db)
            .await
            .unwrap();
        assert!(
            crate::tree::export(&data, &root.path().join("collision"), None, false)
                .await
                .is_err()
        );
        app.stop().await;
    }
    #[test]
    fn cache_identity_includes_selection_media_and_decryption() {
        let mut c = Config::default();
        let selector = Selector::default();
        let old = crypto::digest(
            &serde_json::to_vec(&("snapshot", "key", "json-png-aac-h264-v1")).unwrap(),
        );
        let base = export_id("snapshot", "key", &c, &selector, false, "ffmpeg-a");
        assert_ne!(base, old);
        assert_ne!(
            base,
            export_id("snapshot", "key", &c, &selector, true, "ffmpeg-a")
        );
        assert_ne!(
            base,
            export_id("snapshot", "key", &c, &selector, false, "ffmpeg-b")
        );
        assert_ne!(
            base,
            export_id(
                "snapshot",
                "key",
                &c,
                &Selector {
                    location_id: Some(1),
                    expected_type: None
                },
                false,
                "ffmpeg-a"
            )
        );
        c.usm_decryption_overrides
            .insert("key".into(), crate::usm::Decryption::Plaintext);
        assert_ne!(
            base,
            export_id("snapshot", "key", &c, &selector, false, "ffmpeg-a")
        );
    }
    #[test]
    fn failed_publication_is_removed_but_committed_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("export");
        std::fs::create_dir(&path).unwrap();
        {
            let _guard = PendingPublication(Some(path.clone()));
        }
        assert!(!path.exists());
        std::fs::create_dir(&path).unwrap();
        {
            let mut guard = PendingPublication(Some(path.clone()));
            guard.0 = None;
        }
        assert!(path.exists());
    }
}
