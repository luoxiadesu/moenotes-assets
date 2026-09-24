//! Explicit copy/hardlink export to a readable, immutable object-key tree.
use crate::{crypto, service::Manifest};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Object {
    pub object_key: String,
    pub asset_key: String,
    pub artifact_id: String,
    pub label: String,
    pub content_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub snapshot: String,
    pub profile: String,
    pub role: String,
    pub export_id: String,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    pub schema: String,
    pub naming: String,
    pub objects: Vec<Object>,
    #[serde(default)]
    pub empty_exports: Vec<serde_json::Value>,
}
pub fn component(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_. ()[]".contains(&b) {
            out.push(b as char)
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        out = format!("_{}", out.replace('.', "%2E"));
    }
    while out.ends_with(['.', ' ']) {
        let b = out.pop().unwrap();
        out.push_str(if b == '.' { "%2E" } else { "%20" });
    }
    let stem = out.split('.').next().unwrap().to_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
    {
        out.insert(0, '_');
    }
    if out.len() > 160 {
        out.truncate(140);
        out.push('~');
        out.push_str(&crypto::digest(value.as_bytes())[..16]);
    }
    out
}
pub fn verify(path: &Path, bytes: u64, sha: &str) -> Result<()> {
    ensure!(
        !path.is_symlink() && path.is_file() && path.metadata()?.len() == bytes,
        "object file/size mismatch"
    );
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    ensure!(
        hex::encode(hash.finalize()) == sha,
        "object SHA256 mismatch"
    );
    Ok(())
}
pub(crate) fn safe_destination(root: &Path, relative: &str) -> Result<PathBuf> {
    ensure!(
        relative.len() <= 1024
            && !relative.contains(['\\', ':', '\0'])
            && relative
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != "..")
            && !Path::new(relative).is_absolute(),
        "unsafe tree path"
    );
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
        ensure!(!path.is_symlink(), "tree symlink refused");
    }
    Ok(path)
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().context("tree parent")?)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}
pub async fn export(
    data: &Path,
    destination: &Path,
    snapshot: Option<&str>,
    hardlink: bool,
) -> Result<Index> {
    export_profile(
        data,
        destination,
        snapshot,
        hardlink,
        crate::worker::PROFILE,
    )
    .await
}
pub async fn export_profile(
    data: &Path,
    destination: &Path,
    snapshot: Option<&str>,
    hardlink: bool,
    profile: &str,
) -> Result<Index> {
    use sqlx::{
        Row,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    };
    ensure!(data.exists(), "data directory missing");
    std::fs::create_dir_all(destination)?;
    ensure!(!destination.is_symlink(), "tree root symlink refused");
    let root = std::fs::canonicalize(destination)?;
    let data = std::fs::canonicalize(data)?;
    ensure!(
        !root.starts_with(&data) && !data.starts_with(&root),
        "tree and service storage must be separate"
    );
    let meta = safe_destination(&root, "_meta")?;
    std::fs::create_dir_all(&meta)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(safe_destination(&root, "_meta/tree.lock")?)?;
    fs2::FileExt::try_lock_exclusive(&lock).context("tree in use")?;
    let index_path = safe_destination(&root, "_meta/manifest.json")?;
    let mut index: Index = if index_path.exists() {
        serde_json::from_slice(&std::fs::read(&index_path)?)?
    } else {
        Index {
            schema: "moenotes-assets-object-index/v2".into(),
            naming: "key-stable-artifact-v2".into(),
            objects: vec![],
            empty_exports: vec![],
        }
    };
    ensure!(
        index.schema == "moenotes-assets-object-index/v2"
            && index.naming == "key-stable-artifact-v2",
        "tree naming migration required"
    );
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(data.join("index.sqlite"))
                .read_only(true),
        )
        .await?;
    let rows = sqlx::query("SELECT body FROM exports ORDER BY id")
        .fetch_all(&db)
        .await?;
    db.close().await;
    let mut manifests = vec![];
    for row in rows {
        let m: Manifest = serde_json::from_str(row.get("body"))?;
        if snapshot.is_none_or(|s| m.snapshot == s) && m.profile == profile {
            manifests.push(m);
        }
    }
    manifests.sort_by(|a, b| a.key.cmp(&b.key).then(a.id.cmp(&b.id)));
    let mut existing: BTreeMap<String, Object> = index
        .objects
        .iter()
        .map(|o| (o.object_key.to_ascii_lowercase(), o.clone()))
        .collect();
    ensure!(existing.len() == index.objects.len(), "tree case collision");
    let mut logical: BTreeMap<String, String> = BTreeMap::new();
    for old in &index.objects {
        logical.insert(old.asset_key.clone(), old.export_id.clone());
        verify(
            &safe_destination(&root, &old.object_key)?,
            old.bytes,
            &old.sha256,
        )?;
    }
    for empty in &index.empty_exports {
        let key = empty["asset_key"].as_str().context("empty export key")?;
        let id = empty["export_id"].as_str().context("empty export ID")?;
        logical.insert(key.into(), id.into());
    }
    // Preflight every name and content before creating any output. Multiple versions
    // of one logical key require an explicit snapshot/fresh tree, never overwriting URLs.
    let mut directories = BTreeMap::new();
    for key in manifests
        .iter()
        .map(|m| m.key.as_str())
        .chain(index.objects.iter().map(|o| o.asset_key.as_str()))
    {
        let mut encoded = String::new();
        let mut original = String::new();
        for part in key.split('/') {
            if !encoded.is_empty() {
                encoded.push('/');
                original.push('/');
            }
            encoded.push_str(&component(part));
            original.push_str(part);
            if let Some(old) = directories.insert(encoded.to_ascii_lowercase(), original.clone()) {
                ensure!(
                    old == original,
                    "asset key directory case/encoding collision"
                );
            }
        }
    }
    let mut planned = vec![];
    for m in &manifests {
        if let Some(prior) = logical.insert(m.key.clone(), m.id.clone()) {
            ensure!(
                prior == m.id,
                "multiple exports for one key; choose snapshot and a fresh tree for migration"
            );
        }
        if m.empty && !index.empty_exports.iter().any(|v| v["export_id"] == m.id) {
            index.empty_exports.push(serde_json::json!({"asset_key":m.key,"export_id":m.id,"snapshot":m.snapshot,"profile":m.profile,"status":"empty"}));
        }
        let parts: Vec<_> = m.key.split('/').map(component).collect();
        ensure!(
            parts
                .first()
                .is_some_and(|p| !p.eq_ignore_ascii_case("_meta")),
            "reserved _meta asset key"
        );
        let directory = parts.join("/");
        for f in &m.files {
            let ext = Path::new(&f.artifact.name)
                .extension()
                .and_then(|v| v.to_str())
                .context("artifact extension")?;
            ensure!(
                ext.bytes().all(|b| b.is_ascii_alphanumeric()) && ext.len() <= 12,
                "unsafe artifact extension"
            );
            let stable = f.artifact.metadata["stable_id"].as_str().unwrap_or(&f.id);
            let role = f.artifact.metadata["role"].as_str().unwrap_or("asset");
            let base = parts.last().context("empty key")?;
            let name = if role == "alpha-mask" {
                format!("{base}.alpha.mkv")
            } else if role == "color" || role == "video" {
                format!("{base}.{ext}")
            } else {
                format!(
                    "{}~{}.{ext}",
                    component(&f.artifact.label),
                    &crypto::digest(stable.as_bytes())[..16]
                )
            };
            let object_key = format!("{directory}/{name}");
            let to = safe_destination(&root, &object_key)?;
            let o = Object {
                object_key: object_key.clone(),
                asset_key: m.key.clone(),
                artifact_id: f.id.clone(),
                label: f.artifact.label.clone(),
                content_type: f.artifact.media_type.clone(),
                bytes: f.artifact.bytes,
                sha256: f.artifact.sha256.clone(),
                snapshot: m.snapshot.clone(),
                profile: m.profile.clone(),
                role: role.into(),
                export_id: m.id.clone(),
            };
            ensure!(
                !m.id.is_empty()
                    && m.id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                    && Path::new(&f.artifact.name)
                        .file_name()
                        .is_some_and(|n| n == f.artifact.name.as_str()),
                "unsafe source identity"
            );
            let source = safe_destination(&data, &format!("exports/{}/{}", m.id, f.artifact.name))?;
            verify(&source, o.bytes, &o.sha256)?;
            if let Some(old) = existing.get(&object_key.to_ascii_lowercase()) {
                ensure!(
                    old == &o,
                    "tree path/case/content collision; explicit migration required"
                );
                continue;
            }
            if to.exists() {
                verify(&to, o.bytes, &o.sha256)?;
            }
            existing.insert(object_key.to_ascii_lowercase(), o.clone());
            planned.push((source, to, o));
        }
    }
    for (source, to, o) in planned {
        std::fs::create_dir_all(to.parent().unwrap())?;
        safe_destination(&root, &o.object_key)?;
        if !to.exists() {
            if hardlink {
                std::fs::hard_link(&source, &to)
                    .context("hardlink failed; use copy across filesystems")?;
            } else {
                let mut temp = tempfile::NamedTempFile::new_in(to.parent().unwrap())?;
                std::io::copy(&mut std::fs::File::open(&source)?, &mut temp)?;
                temp.as_file().sync_all()?;
                verify(temp.path(), o.bytes, &o.sha256)?;
                temp.persist_noclobber(&to)?;
            }
        }
        verify(&to, o.bytes, &o.sha256)?;
        index.objects.push(o);
        index
            .objects
            .sort_by(|a, b| a.object_key.cmp(&b.object_key));
        atomic_json(&index_path, &index)?;
    }
    atomic_json(&index_path, &index)?;
    // Public upload plan contains only relative object keys and metadata, no local paths.
    atomic_json(&safe_destination(&root, "_meta/upload-plan.json")?, &index)?;
    Ok(index)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_components() {
        for (a, b) in [
            ("CON", "_CON"),
            ("con.png", "_con.png"),
            ("..", "_%2E%2E"),
            ("a%", "a%25"),
            ("a.", "a%2E"),
            ("é", "%C3%A9"),
        ] {
            assert_eq!(component(a), b);
        }
        assert!(component(&"long".repeat(100)).len() <= 160);
    }
    #[test]
    fn refuses_escapes_and_symlinks() {
        let d = tempfile::tempdir().unwrap();
        for p in ["../a", "/a", "a//b", "C:/x", "a\\b"] {
            assert!(safe_destination(d.path(), p).is_err());
        }
        std::os::unix::fs::symlink("/tmp", d.path().join("x")).unwrap();
        assert!(safe_destination(d.path(), "x/file").is_err());
    }
}
