use moenotes_assets::crypto;
use std::io::Write;

pub const KEY: &str = "Live/MusicScore/test";
pub const INTERNAL: &str = "Assets/fixture.bytes";
pub const BODY: &[u8] = b"{\"fixture\":true}";
fn u32le(b: &mut Vec<u8>, n: u32) {
    b.extend(n.to_le_bytes());
}
fn s(b: &mut Vec<u8>, v: &str) {
    u32le(b, v.len() as u32);
    b.extend(v.as_bytes());
    while !b.len().is_multiple_of(4) {
        b.push(0)
    }
}

pub fn serialized() -> Vec<u8> {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(BODY).unwrap();
    let gzip = gzip.finish().unwrap();
    let mut text = vec![];
    s(&mut text, "fixture");
    u32le(&mut text, gzip.len() as u32);
    text.extend(gzip);
    let mut bundle = vec![];
    s(&mut bundle, "fixture");
    u32le(&mut bundle, 0);
    u32le(&mut bundle, 1);
    s(&mut bundle, INTERNAL);
    u32le(&mut bundle, 0);
    u32le(&mut bundle, 0);
    u32le(&mut bundle, 0);
    bundle.extend(7i64.to_le_bytes());
    bundle.extend([0; 20]);
    u32le(&mut bundle, 0);
    s(&mut bundle, "fixture");
    u32le(&mut bundle, 0);
    bundle.push(0);
    let mut m = vec![];
    m.extend(b"6000.3.12f1\0");
    u32le(&mut m, 13);
    m.push(0);
    u32le(&mut m, 2);
    for class in [49, 142] {
        u32le(&mut m, class);
        m.push(0);
        m.extend((-1i16).to_le_bytes());
        m.extend([0; 16]);
    }
    u32le(&mut m, 2);
    while !(48 + m.len()).is_multiple_of(4) {
        m.push(0)
    }
    for (id, start, size, kind) in [
        (7i64, 0i64, text.len(), 0u32),
        (1, text.len() as i64, bundle.len(), 1),
    ] {
        m.extend(id.to_le_bytes());
        m.extend(start.to_le_bytes());
        u32le(&mut m, size as u32);
        u32le(&mut m, kind);
    }
    for _ in 0..3 {
        u32le(&mut m, 0)
    }
    m.push(0);
    let offset = (48 + m.len()).div_ceil(16) * 16;
    let size = offset + text.len() + bundle.len();
    let mut out = vec![0; 48];
    out[8..12].copy_from_slice(&22u32.to_be_bytes());
    out[20..24].copy_from_slice(&(m.len() as u32).to_be_bytes());
    out[24..32].copy_from_slice(&(size as u64).to_be_bytes());
    out[32..40].copy_from_slice(&(offset as u64).to_be_bytes());
    out.extend(m);
    out.resize(offset, 0);
    out.extend(text);
    out.extend(bundle);
    out
}
pub fn bundle() -> (Vec<u8>, u32) {
    bundle_named(serialized(), "CAB-fixture")
}
pub fn bundle_named(payload: Vec<u8>, name: &str) -> (Vec<u8>, u32) {
    let crc = crc32fast::hash(&payload);
    let mut info = vec![0; 16];
    info.extend(1u32.to_be_bytes());
    info.extend((payload.len() as u32).to_be_bytes());
    info.extend((payload.len() as u32).to_be_bytes());
    info.extend(0u16.to_be_bytes());
    info.extend(1u32.to_be_bytes());
    info.extend(0u64.to_be_bytes());
    info.extend((payload.len() as u64).to_be_bytes());
    info.extend(4u32.to_be_bytes());
    info.extend(name.as_bytes());
    info.push(0);
    let mut out = b"UnityFS\0".to_vec();
    out.extend(8u32.to_be_bytes());
    out.extend(b"5.x.x\0");
    out.extend(b"6000.3.12f1\0");
    let size_at = out.len();
    out.extend([0; 8]);
    out.extend((info.len() as u32).to_be_bytes());
    out.extend((info.len() as u32).to_be_bytes());
    out.extend(0u32.to_be_bytes());
    while !out.len().is_multiple_of(16) {
        out.push(0)
    }
    out.extend(info);
    out.extend(payload);
    let size = out.len() as u64;
    out[size_at..size_at + 8].copy_from_slice(&size.to_be_bytes());
    (out, crc)
}
struct Bin(Vec<u8>);
impl Bin {
    fn put(&mut self, v: &[u8]) -> u32 {
        let n = self.0.len() as u32;
        self.0.extend(v);
        n
    }
    fn words(&mut self, v: &[u32]) -> u32 {
        self.put(&v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<_>>())
    }
    fn string(&mut self, s: &str) -> u32 {
        self.words(&[s.len() as u32]);
        self.put(s.as_bytes())
    }
    fn array(&mut self, v: &[u32]) -> u32 {
        self.words(&[(v.len() * 4) as u32]);
        self.words(v)
    }
    fn typ(&mut self, s: &str) -> u32 {
        let a = self.string("Test");
        let n = self.string(s);
        self.words(&[a, n])
    }
    fn key(&mut self, s: &str) -> u32 {
        let t = self.typ("System.String");
        let s = self.string(s);
        let v = self.words(&[s, 0]);
        self.words(&[t, v])
    }
}
pub fn catalog(size: u64, crc: u32) -> Vec<u8> {
    catalog_with_internal(size, crc, "https://dummy.net/asset/Android/fixture.bundle")
}
pub fn catalog_with_internal(size: u64, crc: u32, internal: &str) -> Vec<u8> {
    let mut b = Bin(vec![0; 32]);
    let h = b.put(&[1; 16]);
    let bn = b.string("fixture");
    let common = b.words(&[0, 0]);
    let opt = b.words(&[h, bn, crc, size as u32, common]);
    let ot = b.typ("UnityEngine.ResourceManagement.ResourceProviders.AssetBundleRequestOptions");
    let extra = b.words(&[ot, opt]);
    let bk = b.string("fixture.bundle");
    let bi = b.string(internal);
    let bp = b.string(moenotes_assets::catalog::CRYPT);
    let bt = b.typ("UnityEngine.ResourceManagement.ResourceProviders.IAssetBundleResource");
    let bundle = b.words(&[bk, bi, bp, u32::MAX, 0, extra, bt]);
    let key = b.string(KEY);
    let internal = b.string(INTERNAL);
    let provider =
        b.string("UnityEngine.ResourceManagement.ResourceProviders.BundledAssetProvider");
    let deps = b.array(&[bundle]);
    let ty = b.typ("UnityEngine.TextAsset");
    let loc = b.words(&[key, internal, provider, deps, 0, u32::MAX, ty]);
    let k = b.key(KEY);
    let ls = b.array(&[loc]);
    let keys = b.array(&[k, ls]);
    b.0[0..4].copy_from_slice(&0x0de38942u32.to_le_bytes());
    b.0[4..8].copy_from_slice(&2u32.to_le_bytes());
    b.0[8..12].copy_from_slice(&keys.to_le_bytes());
    b.0
}
pub fn fixture() -> (Vec<u8>, Vec<u8>) {
    let (mut payload, crc) = bundle();
    let catalog = catalog(payload.len() as u64, crc);
    crypto::decrypt(&mut payload, "fixture.bundle", 0).unwrap();
    (catalog, payload)
}
