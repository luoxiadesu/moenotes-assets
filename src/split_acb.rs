//! SplitAcbLoader-compatible byte assembly: reference order, then XOR 0x5a.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use unity_rs_core::{
    scene::resolve_object_reference,
    serialized::ObjectReference,
    studio::{Studio, StudioObject},
};

pub const TYPE: &str = "Fwk.Sound.SplitAcbData";
fn references(tree: &Value) -> Result<(&str, Vec<ObjectReference>)> {
    let name = tree["_cueSheetName"]
        .as_str()
        .context("split ACB cue sheet name missing")?;
    ensure!(
        !name.is_empty() && name.len() <= 4096,
        "split ACB cue sheet name limit"
    );
    let values = tree["_chunks"]
        .as_array()
        .context("split ACB chunks missing")?;
    ensure!(
        !values.is_empty() && values.len() <= 4096,
        "split ACB chunk count"
    );
    let refs = values
        .iter()
        .map(|v| {
            Ok(ObjectReference {
                file_id: i32::try_from(v["m_FileID"].as_i64().context("split ACB file ID")?)?,
                path_id: v["m_PathID"].as_i64().context("split ACB path ID")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        refs.iter().all(|r| r.file_id >= 0 && r.path_id != 0),
        "invalid split ACB reference"
    );
    Ok((name, refs))
}
fn append(out: &mut Vec<u8>, chunk: &[u8], limit: u64) -> Result<()> {
    ensure!(
        !chunk.is_empty()
            && (out.len() as u64)
                .checked_add(chunk.len() as u64)
                .is_some_and(|n| n <= limit),
        "split ACB byte budget or empty chunk"
    );
    out.try_reserve(chunk.len())
        .context("split ACB allocation")?;
    out.extend(chunk.iter().map(|v| v ^ 0x5a));
    Ok(())
}
fn validate(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() >= 32 && bytes.starts_with(b"@UTF"),
        "split ACB reconstructed header invalid"
    );
    let table = u32::from_be_bytes(bytes[4..8].try_into()?) as u64 + 8;
    ensure!(
        table >= 32 && table <= bytes.len() as u64,
        "split ACB table truncated"
    );
    Ok(())
}
pub fn read(
    studio: &Studio,
    object: StudioObject<'_>,
    limit: u64,
) -> Result<(Vec<u8>, String, usize)> {
    ensure!(object.class_id() == 114, "split ACB wrapper class");
    let tree: Value = serde_json::from_slice(&object.read_type_tree_json(false, 1 << 20)?)?;
    let (name, refs) = references(&tree)?;
    let mut bytes = Vec::new();
    for reference in &refs {
        let resolved =
            resolve_object_reference(studio.collection(), object.file_index(), *reference)?
                .context("split ACB chunk reference missing")?;
        let chunk = studio
            .object(resolved.file_index, resolved.object.path_id)
            .context("split ACB chunk object missing")?;
        ensure!(chunk.class_id() == 49, "split ACB chunk is not TextAsset");
        let raw =
            chunk.read_text_bytes(usize::try_from(limit.saturating_sub(bytes.len() as u64))?)?;
        append(&mut bytes, &raw, limit)?;
    }
    validate(&bytes)?;
    Ok((bytes, name.into(), refs.len()))
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn order_xor_boundary_and_budget() {
        let mut acb = vec![0; 32];
        acb[..4].copy_from_slice(b"@UTF");
        acb[4..8].copy_from_slice(&24u32.to_be_bytes());
        let raw: Vec<_> = acb.iter().map(|b| b ^ 0x5a).collect();
        let mut out = vec![];
        append(&mut out, &raw[..3], 32).unwrap();
        append(&mut out, &raw[3..], 32).unwrap();
        assert_eq!(out, acb);
        validate(&out).unwrap();
        assert!(append(&mut out, &[1], 32).is_err());
        assert!(append(&mut vec![], &[], 32).is_err());
        assert!(validate(&out[..31]).is_err());
        let mut reverse = vec![];
        append(&mut reverse, &raw[3..], 32).unwrap();
        append(&mut reverse, &raw[..3], 32).unwrap();
        assert!(validate(&reverse).is_err());
    }
    #[test]
    fn references_preserve_declared_order_and_reject_missing() {
        let tree = json!({"_cueSheetName":"song","_chunks":[{"m_FileID":0,"m_PathID":20},{"m_FileID":2,"m_PathID":-9}]});
        let (_, r) = references(&tree).unwrap();
        assert_eq!((r[0].path_id, r[1].path_id), (20, -9));
        for value in [
            json!({}),
            json!({"_cueSheetName":"x","_chunks":[]}),
            json!({"_cueSheetName":"x","_chunks":[{"m_FileID":0,"m_PathID":0}]}),
        ] {
            assert!(references(&value).is_err());
        }
    }
}
