//! One HTTP-only dependency policy shared by preflight and execution.
use crate::catalog::{CRI, Catalog, Location, Selector};
use anyhow::{Result, ensure};
use serde::Serialize;

pub const POLICY: &str = "http-resources-v1";
#[derive(Clone, Serialize)]
pub struct Plan {
    pub target: Location,
    pub dependencies: Vec<Location>,
    pub omitted_local: Vec<Location>,
    pub archive: bool,
    pub font: bool,
    pub embedded_cri: bool,
    pub supported: bool,
}
pub fn remote(location: &Location) -> bool {
    location.internal.starts_with("https://") || location.internal.starts_with("http://")
}
impl Plan {
    pub fn build(catalog: &Catalog, key: &str, selector: &Selector, archive: bool) -> Result<Self> {
        let mut target = catalog.resolve(key, selector)?.clone();
        target.key = key.into();
        // A texture alias of a font asset is still a font subresource. It must
        // not accidentally enter image conversion because of catalog ordering.
        let font = catalog.keys[key].iter().any(|id| {
            catalog.locations.get(id).is_some_and(|l| {
                matches!(
                    l.resource_type.as_str(),
                    "TMPro.TMP_FontAsset" | "UnityEngine.Font"
                )
            })
        });
        let archive = archive || font;
        let closure = catalog.closure_from(target.id)?;
        let (mut dependencies, omitted_local): (Vec<_>, Vec<_>) =
            closure.into_iter().partition(remote);
        dependencies.sort_by_key(|l| l.id);
        let cri = target.provider == CRI || target.resource_type.starts_with("CriWare.");
        let mut embedded_cri = false;
        if cri && !archive {
            let raw: Vec<_> = dependencies
                .iter()
                .filter(|l| l.provider == CRI)
                .cloned()
                .collect();
            ensure!(raw.len() <= 1, "ambiguous CRI media dependencies");
            if raw.is_empty() {
                embedded_cri = true;
            } else {
                dependencies = raw;
            }
        }
        ensure!(dependencies.len() <= 512, "dependency count limit");
        let supported = archive
            || target.resource_type == crate::split_acb::TYPE
            || cri
            || matches!(
                target.resource_type.as_str(),
                "UnityEngine.TextAsset"
                    | "UnityEngine.Texture2D"
                    | "UnityEngine.Sprite"
                    | "UnityEngine.U2D.SpriteAtlas"
            );
        Ok(Self {
            target,
            dependencies,
            omitted_local,
            archive,
            font,
            embedded_cri,
            supported,
        })
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.supported,
            "unsupported preview type; use archive for HTTP containers"
        );
        ensure!(
            !self.dependencies.is_empty() || self.font,
            "no HTTP resource payload; package-only content has no catalog download URL"
        );
        Ok(())
    }
    pub fn disposition(&self) -> &'static str {
        if self.font {
            if self.dependencies.is_empty() {
                "font-reference"
            } else {
                "font-archive"
            }
        } else if self.archive {
            "http-container-archive"
        } else {
            "preview"
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Options, PLAIN};
    use std::collections::BTreeMap;
    fn fixture() -> Catalog {
        let target = Location {
            id: 1,
            key: "image".into(),
            internal: "Assets/image".into(),
            provider: "bundled".into(),
            resource_type: "UnityEngine.Texture2D".into(),
            dependencies: vec![2, 3],
            options: None,
        };
        let dep = |id, internal: &str| Location {
            id,
            key: format!("bundle-{id}"),
            internal: internal.into(),
            provider: PLAIN.into(),
            resource_type: "bundle".into(),
            dependencies: vec![],
            options: Some(Options {
                hash: "h".into(),
                bundle_name: "b".into(),
                crc: 0,
                size: 1,
            }),
        };
        Catalog {
            keys: BTreeMap::from([("image".into(), vec![1])]),
            locations: BTreeMap::from([
                (1, target),
                (2, dep(2, "https://cdn.invalid/asset/Android/a.bundle")),
                (3, dep(3, "{RuntimePath}/script.bundle")),
            ]),
        }
    }
    #[test]
    fn only_http_inputs_but_omissions_remain_visible() {
        let c = fixture();
        for archive in [false, true] {
            let p = Plan::build(&c, "image", &Selector::default(), archive).unwrap();
            p.validate().unwrap();
            assert_eq!(p.dependencies.iter().map(|l| l.id).collect::<Vec<_>>(), [2]);
            assert_eq!(p.omitted_local[0].id, 3);
        }
    }
    #[test]
    fn font_aliases_are_retained_even_when_selecting_texture() {
        let mut c = fixture();
        let mut font = c.locations[&1].clone();
        font.id = 4;
        font.resource_type = "TMPro.TMP_FontAsset".into();
        c.locations.insert(4, font);
        c.keys.get_mut("image").unwrap().push(4);
        let p = Plan::build(
            &c,
            "image",
            &Selector {
                expected_type: Some("UnityEngine.Texture2D".into()),
                location_id: None,
            },
            false,
        )
        .unwrap();
        assert!(p.font && p.archive);
        assert_eq!(p.disposition(), "font-archive");
        c.locations.get_mut(&2).unwrap().internal = "{RuntimePath}/font.bundle".into();
        let p = Plan::build(&c, "image", &Selector::default(), false).unwrap();
        p.validate().unwrap();
        assert_eq!(p.disposition(), "font-reference");
    }
    #[test]
    fn no_http_image_is_not_a_successful_empty_export() {
        let mut c = fixture();
        c.locations.get_mut(&2).unwrap().internal = "{RuntimePath}/texture.bundle".into();
        let p = Plan::build(&c, "image", &Selector::default(), false).unwrap();
        assert!(p.validate().is_err());
    }
}
