//! An explicit, hash-pinned read-only directory provider. No local URL guessing.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub root: PathBuf,
    pub entries: BTreeMap<String, Entry>,
}
impl Source {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.entries.len() <= 100_000, "local source entry limit");
        for entry in self.entries.values() {
            validate_relative(&entry.path)?;
            ensure!(
                entry.bytes > 0
                    && entry.sha256.len() == 64
                    && entry.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid local source identity"
            );
        }
        Ok(())
    }
    pub fn identity(&self) -> String {
        crate::crypto::digest(&serde_json::to_vec(&self.entries).unwrap())
    }
    pub fn open(&self, internal: &str) -> Result<(std::fs::File, &Entry)> {
        let entry = self
            .entries
            .get(internal)
            .context("local dependency missing from configured source")?;
        validate_relative(&entry.path)?;
        let root = std::fs::canonicalize(&self.root).context("local source root unavailable")?;
        let mut path = root.clone();
        for part in Path::new(&entry.path).components() {
            path.push(part);
            ensure!(!path.is_symlink(), "local source symlink refused");
        }
        let path = std::fs::canonicalize(path).context("local dependency file missing")?;
        ensure!(path.starts_with(&root), "local source escape");
        let file = std::fs::File::open(path).context("local dependency open failed")?;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() == entry.bytes,
            "local dependency size mismatch"
        );
        Ok((file, entry))
    }
    pub fn copy_verified(&self, internal: &str, destination: &Path, limit: u64) -> Result<String> {
        let (mut input, entry) = self.open(internal)?;
        ensure!(entry.bytes <= limit, "local input budget");
        let mut output = std::fs::File::create(destination)?;
        use sha2::Digest;
        use std::io::Write;
        let mut hash = sha2::Sha256::new();
        let mut received = 0u64;
        let mut buffer = [0; 65536];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            received += n as u64;
            ensure!(received <= entry.bytes, "local input grew");
            hash.update(&buffer[..n]);
            output.write_all(&buffer[..n])?;
        }
        let sha = hex::encode(hash.finalize());
        ensure!(
            received == entry.bytes && sha.eq_ignore_ascii_case(&entry.sha256),
            "local dependency SHA256 mismatch"
        );
        output.sync_all()?;
        Ok(sha)
    }
}
fn validate_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains(['\\', ':', '\0'])
            && Path::new(path)
                .components()
                .all(|v| matches!(v, std::path::Component::Normal(_)))
            && !path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == ".."),
        "unsafe local source path"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_hash_symlink_and_missing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), b"raw").unwrap();
        let mut s = Source {
            root: d.path().into(),
            entries: BTreeMap::from([(
                "local".into(),
                Entry {
                    path: "x".into(),
                    sha256: crate::crypto::digest(b"raw"),
                    bytes: 3,
                },
            )]),
        };
        s.copy_verified("local", &d.path().join("out"), 10).unwrap();
        assert!(s.open("unknown").is_err());
        s.entries.get_mut("local").unwrap().sha256 = "0".repeat(64);
        assert!(s.copy_verified("local", &d.path().join("out"), 10).is_err());
        for p in ["../x", "/x", "a/../x", "a//x", "C:/x", "a\\x"] {
            assert!(validate_relative(p).is_err());
        }
        std::os::unix::fs::symlink("x", d.path().join("link")).unwrap();
        s.entries.get_mut("local").unwrap().path = "link".into();
        assert!(s.open("local").is_err());
    }
}
