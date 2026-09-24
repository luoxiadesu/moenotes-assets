//! Exact Unity 6000 serialized-bytes CRI wrapper; avoids one heap value per byte.
use anyhow::{Context, Result, ensure};
use unity_rs_core::{
    serialized::{SerializedFile, TypeTree},
    studio::StudioObject,
};
const CLASS: &str = "CriSerializedBytesAssetImpl";
const NAMESPACE: &str = "CriWare.Assets";
const ASSEMBLY: &str = "CriMw.CriWare.Assets.Runtime";
fn schema(tree: &TypeTree, expected: &[(&str, &str, u32, i32, bool)]) -> bool {
    tree.nodes.len() == expected.len()
        && tree
            .nodes
            .iter()
            .zip(expected)
            .all(|(n, (ty, name, level, size, align))| {
                n.type_name == *ty
                    && n.field_name == *name
                    && n.level == *level
                    && n.byte_size == *size
                    && (n.meta_flags & 0x4000 != 0) == *align
            })
}
pub fn read(file: &SerializedFile, object: StudioObject<'_>, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        file.header.endianness == 0,
        "embedded CRI little-endian layout required"
    );
    let info = file
        .objects
        .get(object.object_index())
        .context("embedded object index")?;
    let kind = file
        .types
        .get(
            info.serialized_type_index
                .context("embedded type missing")?,
        )
        .context("embedded type index")?;
    ensure!(
        kind.type_tree.as_ref().is_some_and(|t| schema(t, WRAPPER)),
        "unsupported embedded CRI wrapper schema"
    );
    let types: Vec<_> = file
        .reference_types
        .iter()
        .filter(|t| {
            t.class_name.as_deref() == Some(CLASS)
                && t.namespace.as_deref() == Some(NAMESPACE)
                && t.assembly_name.as_deref() == Some(ASSEMBLY)
        })
        .collect();
    ensure!(
        types.len() == 1
            && types[0]
                .type_tree
                .as_ref()
                .is_some_and(|t| schema(t, BYTES)),
        "unsupported embedded CRI byte schema"
    );
    let raw = object.read_raw(limit)?;
    extract(&raw, limit)
}
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .context("embedded offset overflow")?;
        let value = self
            .bytes
            .get(self.pos..end)
            .context("truncated embedded CRI")?;
        self.pos = end;
        Ok(value)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn align(&mut self) -> Result<()> {
        let end = self.pos.div_ceil(4) * 4;
        ensure!(
            self.take(end - self.pos)?.iter().all(|v| *v == 0),
            "nonzero embedded alignment"
        );
        Ok(())
    }
    fn string(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        ensure!(n <= 4096, "embedded string budget");
        let value = std::str::from_utf8(self.take(n)?)?.to_string();
        self.align()?;
        Ok(value)
    }
}
fn extract(raw: &[u8], limit: u64) -> Result<Vec<u8>> {
    let mut r = Reader { bytes: raw, pos: 0 };
    r.take(12)?;
    ensure!(r.take(1)?[0] <= 1, "invalid behaviour enabled");
    r.align()?;
    r.take(12)?;
    r.string()?;
    let rid = r.i64()?;
    ensure!(
        r.take(12)?.iter().all(|v| *v == 0),
        "external AWB reference unsupported"
    );
    ensure!(r.u32()? == 0, "additional AWB references unsupported");
    ensure!(
        r.u32()? == 2 && r.u32()? == 1 && r.i64()? == rid,
        "unsupported embedded reference registry"
    );
    ensure!(
        r.string()? == CLASS && r.string()? == NAMESPACE && r.string()? == ASSEMBLY,
        "unsupported embedded implementation"
    );
    let n = r.u32()? as usize;
    ensure!(n > 0 && n as u64 <= limit, "embedded payload budget");
    let payload = r.take(n)?;
    r.align()?;
    ensure!(
        r.pos == raw.len(),
        "unexpected embedded wrapper trailing bytes"
    );
    ensure!(
        payload.starts_with(b"@UTF") || payload.starts_with(b"CRID"),
        "embedded CRI payload signature"
    );
    Ok(payload.to_vec())
}
const WRAPPER: &[(&str, &str, u32, i32, bool)] = &[
    ("MonoBehaviour", "Base", 0, -1, false),
    ("PPtr<GameObject>", "m_GameObject", 1, 12, false),
    ("int", "m_FileID", 2, 4, false),
    ("SInt64", "m_PathID", 2, 8, false),
    ("UInt8", "m_Enabled", 1, 1, true),
    ("PPtr<MonoScript>", "m_Script", 1, 12, false),
    ("int", "m_FileID", 2, 4, false),
    ("SInt64", "m_PathID", 2, 8, false),
    ("string", "m_Name", 1, -1, false),
    ("Array", "Array", 2, -1, true),
    ("int", "size", 3, 4, false),
    ("char", "data", 3, 1, false),
    ("managedReference", "implementation", 1, 8, false),
    ("SInt64", "rid", 2, 8, false),
    ("PPtr<$CriAtomAwbAsset>", "awb", 1, 12, false),
    ("int", "m_FileID", 2, 4, false),
    ("SInt64", "m_PathID", 2, 8, false),
    ("vector", "additionalAwbs", 1, -1, false),
    ("Array", "Array", 2, -1, false),
    ("int", "size", 3, 4, false),
    ("PPtr<$CriAtomAwbAsset>", "data", 3, 12, false),
    ("int", "m_FileID", 4, 4, false),
    ("SInt64", "m_PathID", 4, 8, false),
    ("ManagedReferencesRegistry", "references", 1, -1, false),
    ("int", "version", 2, 4, false),
    ("vector", "RefIds", 2, -1, false),
    ("Array", "Array", 3, -1, true),
    ("int", "size", 4, 4, false),
    ("ReferencedObject", "data", 4, -1, false),
    ("SInt64", "rid", 5, 8, false),
    ("ReferencedManagedType", "type", 5, -1, false),
    ("string", "class", 6, -1, false),
    ("Array", "Array", 7, -1, true),
    ("int", "size", 8, 4, false),
    ("char", "data", 8, 1, false),
    ("string", "ns", 6, -1, false),
    ("Array", "Array", 7, -1, true),
    ("int", "size", 8, 4, false),
    ("char", "data", 8, 1, false),
    ("string", "asm", 6, -1, false),
    ("Array", "Array", 7, -1, true),
    ("int", "size", 8, 4, false),
    ("char", "data", 8, 1, false),
    ("ReferencedObjectData", "data", 5, 0, false),
];
const BYTES: &[(&str, &str, u32, i32, bool)] = &[
    ("CriSerializedBytesAssetImpl", "Base", 0, -1, false),
    ("vector", "data", 1, -1, true),
    ("Array", "Array", 2, -1, true),
    ("int", "size", 3, 4, false),
    ("UInt8", "data", 3, 1, false),
];
#[cfg(test)]
mod tests {
    use super::*;
    fn string(b: &mut Vec<u8>, s: &str) {
        b.extend((s.len() as u32).to_le_bytes());
        b.extend(s.as_bytes());
        b.resize(b.len().div_ceil(4) * 4, 0);
    }
    fn fixture(n: usize) -> Vec<u8> {
        let mut b = vec![0; 28];
        b[12] = 1;
        string(&mut b, "synthetic");
        b.extend(7i64.to_le_bytes());
        b.extend([0; 12]);
        b.extend(0u32.to_le_bytes());
        b.extend(2u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend(7i64.to_le_bytes());
        for v in [CLASS, NAMESPACE, ASSEMBLY] {
            string(&mut b, v);
        }
        b.extend((n as u32).to_le_bytes());
        b.extend(b"@UTF");
        b.resize(b.len() + n - 4, 0);
        b.resize(b.len().div_ceil(4) * 4, 0);
        b
    }
    #[test]
    fn bounded_byte_array_and_bad_registry() {
        for n in [4, 5, 10_000_000] {
            let b = fixture(n);
            assert_eq!(extract(&b, 20_000_000).unwrap().len(), n);
            assert!(extract(&b, n as u64 - 1).is_err());
            assert!(extract(&b[..b.len() - 1], 20_000_000).is_err());
        }
        let mut b = fixture(4);
        b.push(0);
        assert!(extract(&b, 100).is_err());
        let at = b
            .windows(CLASS.len())
            .position(|v| v == CLASS.as_bytes())
            .unwrap();
        b[at] = b'X';
        assert!(extract(&b, 100).is_err());
    }
}
