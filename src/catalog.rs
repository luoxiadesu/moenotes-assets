use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const CRYPT: &str = "Fwk.Crypt.AssetBundleCryptProvider";
pub const PLAIN: &str = "UnityEngine.ResourceManagement.ResourceProviders.AssetBundleProvider";
pub const CRI: &str = "CriWare.Assets.CriResourceProvider";
const NULL: u32 = u32::MAX;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    pub expected_type: Option<String>,
    pub location_id: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Options {
    pub hash: String,
    pub bundle_name: String,
    pub crc: u32,
    pub size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Location {
    pub id: u32,
    pub key: String,
    pub internal: String,
    pub provider: String,
    pub resource_type: String,
    pub dependencies: Vec<u32>,
    pub options: Option<Options>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Catalog {
    pub keys: BTreeMap<String, Vec<u32>>,
    pub locations: BTreeMap<u32, Location>,
}

struct Reader<'a> {
    data: &'a [u8],
    strings: HashMap<(u32, char), String>,
    string_bytes: usize,
}
impl Reader<'_> {
    fn take(&self, o: u32, n: usize) -> Result<&[u8]> {
        self.data
            .get(o as usize..(o as usize).checked_add(n).context("offset overflow")?)
            .context("catalog out of bounds")
    }
    fn u32(&self, o: u32) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(o, 4)?.try_into()?))
    }
    fn array(&self, o: u32, stride: usize) -> Result<Vec<u32>> {
        if o == NULL {
            return Ok(vec![]);
        }
        let size = self.u32(o.checked_sub(4).context("array offset")?)? as usize;
        ensure!(
            size.is_multiple_of(stride) && size / stride <= 200_000,
            "catalog array limit"
        );
        self.take(o, size)?;
        Ok((0..size / stride)
            .map(|i| o + (i * stride) as u32)
            .collect())
    }
    fn string(&mut self, o: u32, sep: char) -> Result<String> {
        if o == NULL {
            return Ok(String::new());
        }
        if let Some(s) = self.strings.get(&(o, sep)) {
            return Ok(s.clone());
        }
        let value = if o & 0x4000_0000 != 0 {
            ensure!(sep != '\0', "dynamic string without separator");
            let mut p = o;
            let mut seen = BTreeSet::new();
            let mut parts = vec![];
            let mut bytes = 0;
            while p != NULL {
                let a = p & 0x3fff_ffff;
                ensure!(seen.insert(a) && seen.len() <= 4096, "cyclic/long string");
                let s = self.u32(a)?;
                ensure!(s & 0x4000_0000 == 0 || s == NULL, "nested dynamic string");
                let part = self.string(s, '\0')?;
                bytes += part.len();
                ensure!(bytes < 65536, "string too long");
                parts.push(part);
                p = self.u32(a + 4)?;
            }
            parts.reverse();
            parts.join(&sep.to_string())
        } else {
            let a = o & 0x3fff_ffff;
            let n = self.u32(a.checked_sub(4).context("string offset")?)? as usize;
            ensure!(n <= 65536, "string limit");
            let b = self.take(a, n)?;
            if o & 0x8000_0000 != 0 {
                ensure!(n.is_multiple_of(2), "UTF16 size");
                String::from_utf16(
                    &b.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|v| u16::from_le_bytes([v[0], v[1]]))
                        .collect::<Vec<_>>(),
                )?
            } else {
                ensure!(b.is_ascii(), "catalog ASCII string");
                String::from_utf8(b.to_vec())?
            }
        };
        self.string_bytes += value.len();
        ensure!(self.string_bytes <= 64 << 20, "catalog string budget");
        self.strings.insert((o, sep), value.clone());
        Ok(value)
    }
    fn type_name(&mut self, o: u32) -> Result<String> {
        ensure!(o != NULL, "null type");
        self.take(o, 8)?;
        self.string(self.u32(o + 4)?, '.')
    }
    fn key(&mut self, o: u32) -> Result<Option<String>> {
        self.take(o, 8)?;
        let kind = self.type_name(self.u32(o)?)?;
        let at = self.u32(o + 4)?;
        match kind.as_str() {
            "System.String" => {
                self.take(at, 6)?;
                let sep = u16::from_le_bytes(self.take(at + 4, 2)?.try_into()?);
                Ok(Some(self.string(
                    self.u32(at)?,
                    char::from_u32(sep.into()).context("separator")?,
                )?))
            }
            "System.Int32" | "System.Int64" | "System.Boolean" => Ok(None),
            _ => bail!("unsupported key type {kind}"),
        }
    }
    fn location(&mut self, id: u32) -> Result<Location> {
        self.take(id, 28)?;
        let key = self.string(self.u32(id)?, '/')?;
        let internal = self.string(self.u32(id + 4)?, '/')?;
        let provider = self.string(self.u32(id + 8)?, '.')?;
        let dependencies = self
            .array(self.u32(id + 12)?, 4)?
            .into_iter()
            .map(|a| self.u32(a))
            .collect::<Result<Vec<_>>>()?;
        let resource_type = self.type_name(self.u32(id + 24)?)?;
        let extra = self.u32(id + 20)?;
        let options = if extra == NULL {
            None
        } else {
            self.take(extra, 8)?;
            ensure!(
                self.type_name(self.u32(extra)?)?
                    == "UnityEngine.ResourceManagement.ResourceProviders.AssetBundleRequestOptions",
                "unsupported options"
            );
            let a = self.u32(extra + 4)?;
            self.take(a, 20)?;
            let hash = hex::encode(self.take(self.u32(a)?, 16)?);
            Some(Options {
                hash,
                bundle_name: self.string(self.u32(a + 4)?, '_')?,
                crc: self.u32(a + 8)?,
                size: self.u32(a + 12)?.into(),
            })
        };
        ensure!(
            !internal.is_empty() && !provider.is_empty(),
            "empty location"
        );
        Ok(Location {
            id,
            key,
            internal,
            provider,
            resource_type,
            dependencies,
            options,
        })
    }
}
impl Catalog {
    pub fn parse(data: &[u8]) -> Result<Self> {
        ensure!(data.len() <= 32 << 20, "catalog size limit");
        let mut r = Reader {
            data,
            strings: HashMap::new(),
            string_bytes: 0,
        };
        ensure!(
            r.u32(0)? == 0x0de38942 && r.u32(4)? == 2,
            "unsupported catalog format"
        );
        r.take(0, 32)?;
        let mut keys = BTreeMap::new();
        let mut pending = BTreeSet::new();
        let mut references = 0usize;
        let mut materialized = 0usize;
        for a in r.array(r.u32(8)?, 8)? {
            let ids = r
                .array(r.u32(a + 4)?, 4)?
                .into_iter()
                .map(|p| r.u32(p))
                .collect::<Result<Vec<_>>>()?;
            references += ids.len();
            ensure!(references <= 1_000_000, "catalog key reference budget");
            pending.extend(ids.iter().copied());
            if let Some(k) = r.key(r.u32(a)?)? {
                materialized += k.len() + ids.len() * 4;
                ensure!(materialized <= 128 << 20, "catalog materialization budget");
                ensure!(keys.insert(k, ids).is_none(), "duplicate catalog key");
            }
        }
        let mut locations = BTreeMap::new();
        let mut edges = 0usize;
        while let Some(id) = pending.pop_first() {
            if locations.contains_key(&id) {
                continue;
            }
            ensure!(locations.len() < 200_000, "location limit");
            let l = r.location(id)?;
            materialized += l.key.len()
                + l.internal.len()
                + l.provider.len()
                + l.resource_type.len()
                + l.dependencies.len() * 4;
            if let Some(o) = &l.options {
                materialized += o.bundle_name.len() + o.hash.len();
            }
            ensure!(materialized <= 128 << 20, "catalog materialization budget");
            edges += l.dependencies.len();
            ensure!(edges <= 1_000_000, "dependency limit");
            pending.extend(
                l.dependencies
                    .iter()
                    .copied()
                    .filter(|v| !locations.contains_key(v)),
            );
            locations.insert(id, l);
        }
        Ok(Self { keys, locations })
    }
    pub fn target(&self, key: &str) -> Result<&Location> {
        self.resolve(key, &Selector::default())
    }
    pub fn resolve(&self, key: &str, selector: &Selector) -> Result<&Location> {
        let ids = self.keys.get(key).context("asset key not found")?;
        let candidates = ids
            .iter()
            .map(|id| self.locations.get(id).context("location missing"))
            .collect::<Result<Vec<_>>>()?;
        let selected: Vec<_> = candidates
            .into_iter()
            .filter(|l| {
                selector.location_id.is_none_or(|id| id == l.id)
                    && selector
                        .expected_type
                        .as_ref()
                        .is_none_or(|t| t == &l.resource_type)
            })
            .collect();
        let first = *selected
            .first()
            .context("selector did not match a location")?;
        ensure!(
            selected.iter().all(|other| other.internal == first.internal
                && other.provider == first.provider
                && other.dependencies == first.dependencies),
            "ambiguous asset key; select expected_type or location_id"
        );
        Ok(first)
    }
    pub fn closure(&self, key: &str) -> Result<Vec<Location>> {
        self.closure_from(self.target(key)?.id)
    }
    pub fn closure_from(&self, id: u32) -> Result<Vec<Location>> {
        let mut todo = vec![id];
        let mut seen = BTreeSet::new();
        let mut out = vec![];
        while let Some(id) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            let l = self.locations.get(&id).context("dependency missing")?;
            todo.extend(&l.dependencies);
            if l.options.is_some() {
                ensure!(
                    matches!(l.provider.as_str(), CRYPT | PLAIN | CRI),
                    "unsupported provider"
                );
                out.push(l.clone());
            }
        }
        ensure!(!out.is_empty(), "no downloadable dependencies");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_headers() {
        for b in [vec![], vec![0; 32], vec![255; 100]] {
            assert!(Catalog::parse(&b).is_err());
        }
    }
    #[test]
    fn cycles_and_ranges() {
        let mut b = vec![0; 40];
        b[16..20].copy_from_slice(&4u32.to_le_bytes());
        b[20..24].copy_from_slice(&16u32.to_le_bytes());
        let mut r = Reader {
            data: &b,
            strings: HashMap::new(),
            string_bytes: 0,
        };
        assert!(r.string(0x4000_0010, '/').is_err());
        assert!(r.array(0, 4).is_err());
    }
}
