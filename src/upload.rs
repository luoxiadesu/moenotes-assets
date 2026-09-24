//! S3 path-style SigV4 upload with conditional creation and full GET verification.
use crate::{
    crypto,
    tree::{Index, Object},
};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    #[serde(default = "concurrency")]
    pub concurrency: usize,
    #[serde(default = "deadline")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub allow_loopback_http: bool,
}
fn concurrency() -> usize {
    4
}
fn deadline() -> u64 {
    900
}
struct Credentials {
    access: String,
    secret: String,
    token: Option<String>,
}
fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut k = [0; 64];
    let key = if key.len() > 64 {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    k[..key.len()].copy_from_slice(&key);
    let mut inner = Sha256::new();
    inner.update(k.map(|v| v ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(k.map(|v| v ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}
fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char)
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
fn timestamp() -> Result<String> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let input = nix::libc::time_t::try_from(seconds)?;
    let mut value = std::mem::MaybeUninit::<nix::libc::tm>::uninit();
    // gmtime_r writes a caller-owned tm and does not use process-global timezone state.
    let tm = unsafe {
        ensure!(
            !nix::libc::gmtime_r(&input, value.as_mut_ptr()).is_null(),
            "UTC conversion failed"
        );
        value.assume_init()
    };
    Ok(format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    ))
}
struct S3 {
    config: Config,
    credentials: Credentials,
    client: reqwest::Client,
}
impl S3 {
    fn new(config: Config, credentials: Credentials) -> Result<Self> {
        let url = url::Url::parse(&config.endpoint)?;
        ensure!(
            url.scheme() == "https"
                || (config.allow_loopback_http
                    && url.scheme() == "http"
                    && url.host_str() == Some("127.0.0.1")),
            "S3 HTTPS required"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && !config.endpoint.contains(['%', '\\']),
            "invalid S3 endpoint"
        );
        for part in [&config.bucket, &config.region] {
            ensure!(
                !part.is_empty()
                    && part.len() <= 63
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                "invalid S3 bucket/region"
            );
        }
        ensure!(
            (1..=32).contains(&config.concurrency) && (1..=7200).contains(&config.timeout_seconds),
            "invalid upload limits"
        );
        Ok(Self {
            config,
            credentials,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build()?,
        })
    }
    fn request(
        &self,
        method: reqwest::Method,
        key: &str,
        sha: &str,
        extra: BTreeMap<String, String>,
    ) -> Result<reqwest::RequestBuilder> {
        let url = url::Url::parse(&format!(
            "{}/{}/{}",
            self.config.endpoint.trim_end_matches('/'),
            encode(&self.config.bucket),
            key.split('/').map(encode).collect::<Vec<_>>().join("/")
        ))?;
        let stamp = timestamp()?;
        let date = &stamp[..8];
        let scope = format!("{date}/{}/s3/aws4_request", self.config.region);
        let mut headers = extra;
        headers.insert(
            "host".into(),
            url[url::Position::BeforeHost..url::Position::AfterPort].into(),
        );
        headers.insert("x-amz-content-sha256".into(), sha.into());
        headers.insert("x-amz-date".into(), stamp.clone());
        if let Some(token) = &self.credentials.token {
            headers.insert("x-amz-security-token".into(), token.clone());
        }
        let signed = headers.keys().cloned().collect::<Vec<_>>().join(";");
        let canonical_headers = headers
            .iter()
            .map(|(k, v)| {
                format!(
                    "{k}:{}\n",
                    v.split_whitespace().collect::<Vec<_>>().join(" ")
                )
            })
            .collect::<String>();
        let canonical = format!(
            "{}\n{}\n\n{canonical_headers}\n{signed}\n{sha}",
            method.as_str(),
            url.path()
        );
        let sign = format!(
            "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
            crypto::digest(canonical.as_bytes())
        );
        let date_key = hmac(
            format!("AWS4{}", self.credentials.secret).as_bytes(),
            date.as_bytes(),
        );
        let region = hmac(&date_key, self.config.region.as_bytes());
        let service = hmac(&region, b"s3");
        let signing = hmac(&service, b"aws4_request");
        let signature = hex::encode(hmac(&signing, sign.as_bytes()));
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            self.credentials.access
        );
        let mut request = self
            .client
            .request(method, url)
            .header("authorization", auth)
            .timeout(Duration::from_secs(self.config.timeout_seconds));
        for (key, value) in headers {
            request = request.header(key, value);
        }
        Ok(request)
    }
    async fn verify(&self, o: &Object) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::GET,
                &o.object_key,
                &crypto::digest(b""),
                BTreeMap::new(),
            )?
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("S3 verification transport failed"))?;
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "S3 verification HTTP {}",
            response.status()
        );
        ensure!(
            response.content_length().is_none_or(|n| n == o.bytes),
            "S3 verification Content-Length mismatch"
        );
        let mut count = 0u64;
        let mut hash = Sha256::new();
        let mut stream = response.bytes_stream();
        loop {
            let chunk = match stream.try_next().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    let actual = hex::encode(hash.finalize());
                    anyhow::bail!("S3 verification interrupted: received={count}, sha256={actual}");
                }
            };
            count += chunk.len() as u64;
            hash.update(&chunk);
            if count > o.bytes {
                let actual = hex::encode(hash.finalize());
                anyhow::bail!("S3 verification excess bytes: received={count}, sha256={actual}");
            }
        }
        let actual = hex::encode(hash.finalize());
        ensure!(
            count == o.bytes && actual == o.sha256,
            "S3 verification mismatch: received={count}, sha256={actual}"
        );
        Ok(())
    }
    async fn put(&self, path: &Path, o: &Object) -> Result<()> {
        ensure!(
            o.bytes <= 5 * 1024 * 1024 * 1024,
            "single PUT exceeds 5 GiB; multipart unsupported"
        );
        let file = tokio::fs::File::open(path).await?;
        let extra = BTreeMap::from([
            ("if-none-match".into(), "*".into()),
            ("content-type".into(), o.content_type.clone()),
            ("x-amz-meta-sha256".into(), o.sha256.clone()),
        ]);
        let response = self
            .request(reqwest::Method::PUT, &o.object_key, &o.sha256, extra)?
            .header("content-length", o.bytes)
            .body(reqwest::Body::wrap_stream(
                tokio_util::io::ReaderStream::new(file),
            ))
            .send()
            .await
            .map_err(|_| {
                anyhow::anyhow!("S3 upload transport failed; retry uses conditional creation")
            })?;
        ensure!(
            response.status().is_success()
                || response.status() == reqwest::StatusCode::PRECONDITION_FAILED,
            "S3 conditional PUT HTTP {}",
            response.status()
        );
        self.verify(o).await
    }
}
pub async fn upload(tree: &Path, config: Config, cancel: CancellationToken) -> Result<()> {
    let credentials = Credentials {
        access: std::env::var("AWS_ACCESS_KEY_ID").context("AWS_ACCESS_KEY_ID required")?,
        secret: std::env::var("AWS_SECRET_ACCESS_KEY").context("AWS_SECRET_ACCESS_KEY required")?,
        token: std::env::var("AWS_SESSION_TOKEN").ok(),
    };
    let client = Arc::new(S3::new(config, credentials)?);
    let root = std::fs::canonicalize(tree)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(crate::tree::safe_destination(&root, "_meta/tree.lock")?)?;
    fs2::FileExt::try_lock_exclusive(&lock).context("tree in use")?;
    let manifest = crate::tree::safe_destination(&root, "_meta/manifest.json")?;
    ensure!(!manifest.is_symlink(), "manifest symlink refused");
    let index: Index = serde_json::from_slice(&std::fs::read(&manifest)?)?;
    ensure!(
        index.schema == "moenotes-assets-object-index/v2",
        "unsupported upload manifest"
    );
    let mut selected = vec![];
    let mut seen = std::collections::BTreeSet::new();
    for o in index.objects {
        ensure!(seen.insert(o.object_key.clone()), "duplicate upload key");
        let path = crate::tree::safe_destination(&root, &o.object_key)?;
        crate::tree::verify(&path, o.bytes, &o.sha256)?;
        selected.push((path, o));
    }
    let concurrency = client.config.concurrency;
    use std::os::unix::fs::OpenOptionsExt;
    let journal = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(crate::tree::safe_destination(
                &root,
                "_meta/upload-local.jsonl",
            )?)?,
    ));
    let mut stream = futures_util::stream::iter(selected)
        .map(|(path, object)| {
            let client = client.clone();
            let journal = journal.clone();
            let cancel = cancel.clone();
            async move { upload_object(&client, &path, &object, &journal, &cancel).await }
        })
        .buffer_unordered(concurrency);
    let mut errors = 0;
    while let Some(result) = stream.next().await {
        if result.is_err() {
            errors += 1;
        }
    }
    ensure!(
        errors == 0,
        "{errors} objects failed verification; see private upload journal"
    );
    let bytes = std::fs::read(&manifest)?;
    let meta = Object {
        object_key: "_meta/manifest.json".into(),
        asset_key: "_meta".into(),
        artifact_id: "manifest".into(),
        label: "manifest".into(),
        content_type: "application/json".into(),
        bytes: bytes.len() as u64,
        sha256: crypto::digest(&bytes),
        snapshot: String::new(),
        profile: String::new(),
        role: "manifest".into(),
        export_id: String::new(),
    };
    upload_object(&client, &manifest, &meta, &journal, &cancel).await?;
    Ok(())
}
async fn upload_object(
    client: &S3,
    path: &Path,
    object: &Object,
    journal: &Mutex<std::fs::File>,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut failure = None;
    for attempt in 1..=3 {
        let started = std::time::Instant::now();
        let result = tokio::select! {
            _ = cancel.cancelled()=>Err(anyhow::anyhow!("upload cancelled")),
            r=client.put(path,object)=>r,
        };
        {
            let mut file = journal.lock().unwrap();
            serde_json::to_writer(
                &mut *file,
                &serde_json::json!({"object_key":object.object_key,"sha256":object.sha256,"bytes":object.bytes,"attempt":attempt,"verified":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string),"elapsed_ms":started.elapsed().as_millis(),"verification":"full signed GET"}),
            )?;
            file.write_all(b"\n")?;
            file.sync_data()?;
        }
        match result {
            Ok(()) => return Ok(()),
            Err(error) => failure = Some(error),
        };
        if cancel.is_cancelled() {
            break;
        }
        if attempt < 3 {
            tokio::select! {_ = cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(attempt))=>{}}
        }
    }
    Err(failure.context("upload failed")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hmac_rfc4231() {
        assert_eq!(
            hex::encode(hmac(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(encode("a %/é"), "a%20%25%2F%C3%A9");
        assert_eq!(timestamp().unwrap().len(), 16);
        // RFC 4231 long-key case exercises key hashing as well as normal HMAC.
        assert_eq!(
            hex::encode(hmac(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
    #[tokio::test]
    async fn conditional_creation_conflict_and_truncated_readback() {
        use axum::{
            Router,
            body::Body,
            extract::State,
            http::{HeaderMap, Response, StatusCode},
            routing::put,
        };
        type Store = Arc<Mutex<Option<Vec<u8>>>>;
        async fn write(
            State(store): State<Store>,
            h: HeaderMap,
            bytes: axum::body::Bytes,
        ) -> StatusCode {
            assert_eq!(h["if-none-match"], "*");
            assert!(
                h["authorization"]
                    .to_str()
                    .unwrap()
                    .contains("if-none-match")
            );
            let mut s = store.lock().unwrap();
            if s.is_some() {
                StatusCode::PRECONDITION_FAILED
            } else {
                *s = Some(bytes.to_vec());
                StatusCode::OK
            }
        }
        async fn read(State(store): State<Store>) -> Response<Body> {
            Response::new(Body::from(store.lock().unwrap().clone().unwrap()))
        }
        let store = Store::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .route("/bucket/file", put(write).get(read))
                    .with_state(store.clone()),
            )
            .into_future(),
        );
        let s = S3::new(
            Config {
                endpoint: format!("http://{addr}"),
                bucket: "bucket".into(),
                region: "test".into(),
                concurrency: 1,
                timeout_seconds: 2,
                allow_loopback_http: true,
            },
            Credentials {
                access: "test".into(),
                secret: "secret".into(),
                token: None,
            },
        )
        .unwrap();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        std::fs::write(&p, b"hello").unwrap();
        let o = Object {
            object_key: "file".into(),
            asset_key: "a".into(),
            artifact_id: "1".into(),
            label: "a".into(),
            content_type: "text/plain".into(),
            bytes: 5,
            sha256: crypto::digest(b"hello"),
            snapshot: "s".into(),
            profile: "p".into(),
            role: "asset".into(),
            export_id: "e".into(),
        };
        s.put(&p, &o).await.unwrap();
        s.put(&p, &o).await.unwrap();
        *store.lock().unwrap() = Some(b"bad".to_vec());
        assert!(s.put(&p, &o).await.is_err());
        assert_eq!(store.lock().unwrap().as_deref(), Some(b"bad".as_slice()));
        server.abort();
    }
}
