//! Bounded subprocess capture. Raw diagnostics stay on the private service volume.
use crate::config::Config;
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    collections::VecDeque,
    fmt,
    io::{Read, Write},
    os::unix::{fs::OpenOptionsExt, process::ExitStatusExt},
    process::{Command, ExitStatus, Stdio},
    time::Instant,
};
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) const STDERR_LIMIT: usize = 16 * 1024;
const STDOUT_LIMIT: usize = 1024 * 1024;

#[derive(Default, Serialize)]
pub(crate) struct Capture {
    bytes: VecDeque<u8>,
    total: u64,
}
impl Capture {
    fn append(&mut self, bytes: &[u8], limit: usize, tail: bool) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        if tail {
            let data = &bytes[bytes.len().saturating_sub(limit)..];
            let remove = (self.bytes.len() + data.len()).saturating_sub(limit);
            self.bytes.drain(..remove);
            self.bytes.extend(data);
        } else {
            self.bytes
                .extend(&bytes[..bytes.len().min(limit - self.bytes.len())]);
        }
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.iter().copied().collect::<Vec<_>>()).into_owned()
    }
}
fn drain(mut input: impl Read, limit: usize, tail: bool) -> std::io::Result<Capture> {
    let mut result = Capture::default();
    let mut buf = [0; 8192];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            return Ok(result);
        }
        result.append(&buf[..n], limit, tail);
    }
}
pub(crate) async fn drain_stderr(input: impl AsyncRead + Unpin) -> std::io::Result<Capture> {
    drain_async(input, STDERR_LIMIT, true).await
}
async fn drain_async(
    mut input: impl AsyncRead + Unpin,
    limit: usize,
    tail: bool,
) -> std::io::Result<Capture> {
    let mut result = Capture::default();
    let mut buf = [0; 8192];
    loop {
        let n = input.read(&mut buf).await?;
        if n == 0 {
            return Ok(result);
        }
        result.append(&buf[..n], limit, tail);
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ProcessFailure {
    pub code: &'static str,
    pub stage: &'static str,
    pub tool: &'static str,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub elapsed_ms: u128,
    pub diagnostic_id: Option<String>,
}
impl fmt::Display for ProcessFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: tool={}, stage={}, exit_code={:?}, signal={:?}, elapsed_ms={}, diagnostic_id={}",
            self.code,
            self.tool,
            self.stage,
            self.exit_code,
            self.signal,
            self.elapsed_ms,
            self.diagnostic_id.as_deref().unwrap_or("unavailable")
        )
    }
}
impl std::error::Error for ProcessFailure {}

pub(crate) fn failure(
    c: &Config,
    code: &'static str,
    stage: &'static str,
    tool: &'static str,
    status: Option<ExitStatus>,
    start: Instant,
    stderr: &Capture,
) -> ProcessFailure {
    let mut error = ProcessFailure {
        code,
        stage,
        tool,
        exit_code: status.and_then(|s| s.code()),
        signal: status.and_then(|s| s.signal()),
        elapsed_ms: start.elapsed().as_millis(),
        diagnostic_id: None,
    };
    let diagnostic_id = uuid::Uuid::new_v4().to_string();
    let save = || -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = c.data_dir.join("diagnostics");
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        anyhow::ensure!(
            !dir.is_symlink() && dir.is_dir(),
            "invalid diagnostics directory"
        );
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(format!("{diagnostic_id}.json")))?;
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({
                "failure": error, "stderr_tail": stderr.text(), "stderr_bytes": stderr.total,
                "stderr_truncated": stderr.total > STDERR_LIMIT as u64,
                "context": std::env::var("MOENOTES_DIAGNOSTIC_CONTEXT").ok()
                    .filter(|v| v.len() <= 65536)
                    .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok()),
                "tool_versions": std::fs::read(dir.join("tools.json")).ok()
                    .filter(|v| v.len() <= 4096)
                    .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok()),
            }),
        )?;
        file.write_all(b"\n")?;
        Ok(())
    };
    match save() {
        Ok(()) => error.diagnostic_id = Some(diagnostic_id),
        Err(_) => tracing::error!(
            code,
            stage,
            tool,
            "could not save private process diagnostic"
        ),
    }
    error
}

pub(crate) fn capture(
    c: &Config,
    mut command: Command,
    stage: &'static str,
    tool: &'static str,
) -> Result<Vec<u8>> {
    let start = Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| {
            failure(
                c,
                "process_spawn_failed",
                stage,
                tool,
                None,
                start,
                &Capture::default(),
            )
        })?;
    // Drain both pipes concurrently and keep bounded buffers even for malformed tools.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (status, out, err) = std::thread::scope(|scope| {
        let out = scope.spawn(|| drain(stdout, STDOUT_LIMIT, false));
        let err = scope.spawn(|| drain(stderr, STDERR_LIMIT, true));
        (
            child.wait(),
            out.join().expect("stdout drain"),
            err.join().expect("stderr drain"),
        )
    });
    let status = status.context("process wait failed")?;
    let out = out.context("process stdout read failed")?;
    let err = err.context("process stderr read failed")?;
    if !status.success() || out.total > STDOUT_LIMIT as u64 {
        let code = if out.total > STDOUT_LIMIT as u64 {
            "process_output_limit"
        } else if status.signal() == Some(nix::libc::SIGXCPU) {
            "process_cpu_limit"
        } else if status.signal().is_some() {
            "process_signal"
        } else {
            match stage {
                "probe" => "media_probe_failed",
                "verify" => "media_verify_failed",
                "decode" => "media_decode_failed",
                _ => "media_encode_failed",
            }
        };
        return Err(failure(c, code, stage, tool, Some(status), start, &err).into());
    }
    Ok(out.bytes.into_iter().collect())
}

// Startup probes use the same bounded capture policy and a wall deadline.
pub(crate) async fn inspect_tool(
    c: &Config,
    binary: &str,
    args: &[&str],
    tool: &'static str,
) -> Result<Vec<u8>> {
    use tokio::process::Command;
    let started = Instant::now();
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            failure(
                c,
                "process_spawn_failed",
                "startup",
                tool,
                None,
                started,
                &Capture::default(),
            )
        })?;
    let pid = child.id().context("media tool PID")?;
    let out = tokio::spawn(drain_async(
        child.stdout.take().expect("piped stdout"),
        STDOUT_LIMIT,
        false,
    ));
    let err = tokio::spawn(drain_stderr(child.stderr.take().expect("piped stderr")));
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let reaped = child.wait().await.ok();
    let out = out.await.context("tool stdout task")??;
    let err = err.await.context("tool stderr task")??;
    let status = match status {
        Ok(Ok(status)) => status,
        _ => {
            return Err(failure(
                c,
                "process_startup_timeout_or_wait_failed",
                "startup",
                tool,
                reaped,
                started,
                &err,
            )
            .into());
        }
    };
    if !status.success() || out.total > STDOUT_LIMIT as u64 {
        return Err(failure(
            c,
            "process_startup_failed",
            "startup",
            tool,
            Some(status),
            started,
            &err,
        )
        .into());
    }
    Ok(out.bytes.into_iter().collect())
}

pub(crate) fn save_versions(c: &Config, versions: &serde_json::Value) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = c.data_dir.join("diagnostics");
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    anyhow::ensure!(
        !dir.is_symlink() && dir.is_dir(),
        "invalid diagnostics directory"
    );
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer(&mut file, versions)?;
    file.persist(dir.join("tools.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }
    #[test]
    fn bounded_large_stderr_private_details_and_safe_public_summary() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config {
            data_dir: dir.path().into(),
            ..Default::default()
        };
        let error = capture(&c, shell("head -c 2097152 /dev/zero >&2; printf 'secret=/private/input key=sensitive-key-value-1234\\nTAIL' >&2; exit 7"), "encode", "ffmpeg").unwrap_err();
        let public = error.to_string();
        assert!(public.contains("media_encode_failed"));
        assert!(public.contains("exit_code=Some(7)"));
        assert!(
            !public.contains("/private")
                && !public.contains("sensitive-key-value-1234")
                && !public.contains("secret")
        );
        let failure = error.downcast_ref::<ProcessFailure>().unwrap();
        let path = dir
            .path()
            .join("diagnostics")
            .join(format!("{}.json", failure.diagnostic_id.as_ref().unwrap()));
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(data["stderr_tail"].as_str().unwrap().ends_with("TAIL"));
        assert!(data["stderr_tail"].as_str().unwrap().len() <= STDERR_LIMIT);
        assert_eq!(data["stderr_truncated"], true);
        assert!(data["stderr_bytes"].as_u64().unwrap() > 2_000_000);
    }
    #[test]
    fn probe_limit_missing_binary_signals_and_verify_stage() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config {
            data_dir: dir.path().into(),
            ..Default::default()
        };
        for (command, stage, expected) in [
            (
                shell("head -c 2097152 /dev/zero"),
                "probe",
                "process_output_limit",
            ),
            (
                Command::new("/nonexistent/private/tool"),
                "probe",
                "process_spawn_failed",
            ),
            (shell("kill -KILL $$"), "encode", "process_signal"),
            (shell("kill -XCPU $$"), "verify", "process_cpu_limit"),
            (shell("exit 9"), "verify", "media_verify_failed"),
            (shell("exit 4"), "probe", "media_probe_failed"),
        ] {
            let error = capture(&c, command, stage, "fixture").unwrap_err();
            assert!(error.to_string().starts_with(expected), "{error}");
            assert!(!error.to_string().contains("/nonexistent"));
        }
    }
    #[test]
    fn stdout_and_stderr_drained_concurrently_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config {
            data_dir: dir.path().into(),
            ..Default::default()
        };
        assert_eq!(
            capture(
                &c,
                shell("head -c 2097152 /dev/zero >&2; printf ok"),
                "probe",
                "fixture"
            )
            .unwrap(),
            b"ok"
        );
    }
    #[tokio::test]
    async fn async_worker_drain_keeps_tail() {
        let bytes = vec![b'x'; STDERR_LIMIT * 5];
        let result = drain_stderr(bytes.as_slice()).await.unwrap();
        assert_eq!(result.bytes.len(), STDERR_LIMIT);
        assert_eq!(result.total, bytes.len() as u64);
    }
}
