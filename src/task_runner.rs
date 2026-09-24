use crate::service::{ItemResult, Task};
use anyhow::Result;
use std::{collections::HashMap, future::Future};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub(crate) fn failed_item(key: String, error: &str) -> ItemResult {
    ItemResult {
        key,
        export_id: None,
        error: Some(error.into()),
    }
}

pub(crate) fn persistence_failed(task: &mut Task) {
    task.error = Some("task persistence failed; resubmit unsuccessful keys".into());
    task.state = "failed".into();
    task.updated = super::service::now();
}

// Each resource is polled by the runtime even while the coordinator saves a checkpoint.
// Buffering bare futures here can suspend a SQL lease while save_task needs that lease.
pub(crate) async fn run<E, F, P, S>(
    task: &mut Task,
    keys: Vec<String>,
    concurrency: usize,
    token: &CancellationToken,
    execute: E,
    persist: P,
) where
    E: Fn(String) -> F,
    F: Future<Output = ItemResult> + Send + 'static,
    P: Fn(Task) -> S,
    S: Future<Output = Result<()>>,
{
    let mut pending = keys.into_iter();
    let mut active = JoinSet::new();
    let mut identities = HashMap::new();
    loop {
        while active.len() < concurrency && !token.is_cancelled() {
            let Some(key) = pending.next() else { break };
            let future = execute(key.clone());
            let cancel = token.clone();
            let cancelled = key.clone();
            let handle = active.spawn(async move {
                tokio::select! {
                    biased;
                    result = future => result,
                    _ = cancel.cancelled() => failed_item(cancelled, "cancelled"),
                }
            });
            identities.insert(handle.id(), key);
        }
        let Some(joined) = active.join_next_with_id().await else {
            break;
        };
        let result = match joined {
            Ok((id, result)) => {
                identities.remove(&id);
                result
            }
            Err(error) => failed_item(
                identities
                    .remove(&error.id())
                    .expect("tracked resource task"),
                "resource task terminated unexpectedly",
            ),
        };
        task.results.push(result);
        task.completed = task.results.len();
        task.updated = super::service::now();
        if task.error.is_none()
            && task.completed.is_multiple_of(20)
            && let Err(error) = persist(task.clone()).await
        {
            tracing::error!(task=%task.id, %error, "task checkpoint failed");
            persistence_failed(task);
            token.cancel();
        }
    }
    task.results
        .extend(pending.map(|key| failed_item(key, "cancelled")));
    task.completed = task.results.len();
    let successes = task
        .results
        .iter()
        .filter(|r| r.export_id.is_some())
        .count();
    task.state = if task.error.is_some() {
        "failed"
    } else if token.is_cancelled() {
        "cancelled"
    } else if successes == task.total {
        "succeeded"
    } else if successes > 0 {
        "partial"
    } else {
        "failed"
    }
    .into();
    task.updated = super::service::now();
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
    use std::{sync::Arc, time::Duration};
    use tokio::sync::Semaphore;

    fn task(n: usize) -> Task {
        Task {
            id: "test".into(),
            kind: "export".into(),
            state: "running".into(),
            snapshot: None,
            keys: (0..n).map(|i| i.to_string()).collect(),
            total: n,
            completed: 0,
            results: vec![],
            error: None,
            created: 0,
            updated: 0,
        }
    }
    async fn pool(n: u32) -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(n)
            .acquire_timeout(Duration::from_millis(300))
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }
    fn ok(key: String) -> ItemResult {
        ItemResult {
            key,
            export_id: Some("cached".into()),
            error: None,
        }
    }
    // The first checkpoint is only reached once every SQL connection is held by a
    // later resource. Releasing those resources requires polling their futures.
    async fn gated(
        key: String,
        db: SqlitePool,
        held: Arc<Semaphore>,
        release: Arc<Semaphore>,
        n: u32,
    ) -> ItemResult {
        let i: usize = key.parse().unwrap();
        if i == 19 {
            held.acquire_many(n).await.unwrap().forget();
        } else if i >= 20 {
            let _connection = db.acquire().await.unwrap();
            held.add_permits(1);
            release.acquire().await.unwrap().forget();
        }
        ok(key)
    }
    #[tokio::test]
    async fn reproduces_legacy_checkpoint_pool_starvation() {
        let db = pool(4).await;
        let held = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let mut stream = futures_util::stream::iter(0..24)
            .map(|i| gated(i.to_string(), db.clone(), held.clone(), release.clone(), 4))
            .buffer_unordered(8);
        for _ in 0..20 {
            stream.next().await.unwrap();
        }
        release.add_permits(4);
        assert!(matches!(db.acquire().await, Err(sqlx::Error::PoolTimedOut)));
        drop(stream);
    }
    #[tokio::test]
    async fn checkpoint_keeps_sql_holders_running() {
        for n in [1, 4] {
            let db = pool(n).await;
            let held = Arc::new(Semaphore::new(0));
            let release = Arc::new(Semaphore::new(0));
            let mut task = task(20 + n as usize);
            let keys = task.keys.clone();
            let persist = |_: Task| {
                let db = db.clone();
                let release = release.clone();
                async move {
                    release.add_permits(n as usize);
                    sqlx::query("SELECT 1").execute(&db).await?;
                    Ok(())
                }
            };
            tokio::time::timeout(
                Duration::from_secs(3),
                run(
                    &mut task,
                    keys,
                    8,
                    &CancellationToken::new(),
                    |key| gated(key, db.clone(), held.clone(), release.clone(), n),
                    persist,
                ),
            )
            .await
            .unwrap();
            assert_eq!(task.state, "succeeded");
            assert_eq!(task.completed, task.total);
        }
    }
    #[tokio::test]
    async fn batch_matrix_success_fast_failure_and_slow_sql() {
        for size in [1, 4] {
            let db = pool(size).await;
            for concurrency in [1, 8, 16] {
                for count in [20, 100, 1000] {
                    let mut task = task(count);
                    let keys = task.keys.clone();
                    run(
                        &mut task,
                        keys,
                        concurrency,
                        &CancellationToken::new(),
                        |key| {
                            let db = db.clone();
                            async move {
                                let i: usize = key.parse().unwrap();
                                if i.is_multiple_of(7) {
                                    return failed_item(key, "synthetic failure");
                                }
                                let mut conn = db.acquire().await.unwrap();
                                if i.is_multiple_of(11) {
                                    tokio::time::sleep(Duration::from_millis(1)).await;
                                }
                                sqlx::query("SELECT 1").execute(&mut *conn).await.unwrap();
                                ok(key)
                            }
                        },
                        |_| {
                            let db = db.clone();
                            async move {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                                sqlx::query("SELECT 1").execute(&db).await?;
                                Ok(())
                            }
                        },
                    )
                    .await;
                    assert_eq!(task.state, "partial");
                    assert_eq!(task.results.len(), count);
                    assert_eq!(
                        task.results.iter().filter(|r| r.error.is_some()).count(),
                        (count - 1) / 7 + 1
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn checkpoint_failure_cancel_and_panic_account_for_all_keys() {
        for failure in [false, true] {
            let token = CancellationToken::new();
            let mut task = task(100);
            let keys = task.keys.clone();
            run(
                &mut task,
                keys,
                8,
                &token,
                |key| async move {
                    if key == "3" {
                        panic!("synthetic worker panic");
                    }
                    ok(key)
                },
                |_| {
                    let token = token.clone();
                    async move {
                        if failure {
                            anyhow::bail!("injected SQL error");
                        }
                        token.cancel();
                        Ok(())
                    }
                },
            )
            .await;
            assert_eq!(task.state, if failure { "failed" } else { "cancelled" });
            assert_eq!(task.error.is_some(), failure);
            assert_eq!(task.completed, 100);
            let keys: std::collections::HashSet<_> = task.results.iter().map(|r| &r.key).collect();
            assert_eq!(keys.len(), 100);
            assert!(
                task.results
                    .iter()
                    .any(|r| r.error.as_deref() == Some("resource task terminated unexpectedly"))
            );
        }
    }
}
