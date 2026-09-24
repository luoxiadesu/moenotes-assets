//! Bounded USM demuxer, independent of header order and per-stream termination.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decryption {
    #[default]
    Key,
    Plaintext,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub display_width: u32,
    pub display_height: u32,
    pub frames: u32,
    pub fps_n: u32,
    pub fps_d: u32,
    pub codec: u32,
}
impl VideoInfo {
    pub fn fps(&self) -> String {
        format!("{}/{}", self.fps_n, self.fps_d)
    }
    pub fn duration(&self) -> f64 {
        self.frames as f64 * self.fps_d as f64 / self.fps_n as f64
    }
    fn from_table(t: &BTreeMap<String, u64>) -> Result<Self> {
        let get = |key: &str| -> Result<u32> {
            u32::try_from(*t.get(key).with_context(|| format!("missing USM {key}"))?)
                .context("USM integer overflow")
        };
        let v = Self {
            width: get("width")?,
            height: get("height")?,
            display_width: get("disp_width")?,
            display_height: get("disp_height")?,
            frames: get("total_frames")?,
            fps_n: get("framerate_n")?,
            fps_d: get("framerate_d")?,
            codec: get("mpeg_codec")?,
        };
        ensure!(
            v.frames > 0
                && v.frames <= 10_000_000
                && v.fps_n > 0
                && v.fps_d > 0
                && (v.fps_n as u64) <= 240 * (v.fps_d as u64),
            "invalid USM timebase"
        );
        ensure!(
            v.display_width > 0
                && v.display_height > 0
                && v.display_width <= v.width
                && v.display_height <= v.height
                && v.width <= 16384
                && v.height <= 16384,
            "invalid USM dimensions"
        );
        ensure!(matches!(v.codec, 1 | 9), "unsupported USM video codec");
        Ok(v)
    }
}
#[derive(Debug)]
pub struct Stream {
    pub signature: [u8; 4],
    pub channel: u8,
    pub path: PathBuf,
    pub video: Option<VideoInfo>,
    pub audio_codec: Option<u64>,
    pub chunks: usize,
}
struct Pending {
    stream: Stream,
    file: File,
    ended: bool,
    header: bool,
}
fn be16(b: &[u8], p: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(
        b.get(p..p + 2).context("truncated UTF")?.try_into()?,
    ))
}
fn be32(b: &[u8], p: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        b.get(p..p + 4).context("truncated UTF")?.try_into()?,
    ))
}
fn string(b: &[u8], p: usize, end: usize) -> Result<String> {
    let v = b.get(p..end).context("UTF string bounds")?;
    let n = v
        .iter()
        .position(|v| *v == 0)
        .context("unterminated UTF string")?;
    ensure!(n <= 4096, "UTF string limit");
    Ok(std::str::from_utf8(&v[..n])?.into())
}
// Metadata-only UTF reader: all references and row widths are checked, no media
// payload is interpreted as metadata and only integer header fields are retained.
fn table(b: &[u8]) -> Result<(String, BTreeMap<String, u64>)> {
    table_row(b, 0)
}
fn table_row(b: &[u8], row_index: usize) -> Result<(String, BTreeMap<String, u64>)> {
    ensure!(
        b.starts_with(b"@UTF") && b.len() >= 32 && b.len() <= 16 << 20,
        "invalid UTF table"
    );
    let end = 8 + be32(b, 4)? as usize;
    ensure!(end <= b.len() && end >= 32, "UTF length");
    let rows = 8 + be32(b, 8)? as usize;
    let strings = 8 + be32(b, 12)? as usize;
    let data = 8 + be32(b, 16)? as usize;
    ensure!(
        32 <= rows && rows <= strings && strings <= data && data <= end,
        "UTF offsets"
    );
    let name = string(b, strings + be32(b, 20)? as usize, data)?;
    let columns = be16(b, 24)? as usize;
    let stride = be16(b, 26)? as usize;
    let count = be32(b, 28)? as usize;
    ensure!(
        columns <= 256
            && count <= 1_000_000
            && count
                .checked_mul(stride)
                .is_some_and(|v| v <= strings - rows),
        "UTF row bounds"
    );
    ensure!(row_index < count, "UTF row index");
    let mut schema = 32;
    let mut row = rows + row_index * stride;
    let row_end = row + stride;
    let mut out = BTreeMap::new();
    for _ in 0..columns {
        let flags = *b.get(schema).context("UTF column")?;
        let key = string(b, strings + be32(b, schema + 1)? as usize, data)?;
        schema += 5;
        let kind = flags & 15;
        let size = match kind {
            0 | 1 => 1,
            2 | 3 => 2,
            4 | 5 | 8 | 10 => 4,
            6 | 7 | 9 | 11 => 8,
            _ => bail!("UTF column type"),
        };
        let at = match flags & 0xf0 {
            0x10 => None,
            0x30 | 0x70 => {
                let at = schema;
                schema += size;
                ensure!(schema <= rows, "UTF constant bounds");
                Some(at)
            }
            0x50 => {
                let at = row;
                row += size;
                ensure!(row <= row_end && count > 0, "UTF row width");
                Some(at)
            }
            _ => bail!("UTF storage"),
        };
        if let Some(at) = at {
            let value = b.get(at..at + size).context("UTF value bounds")?;
            if kind <= 7 {
                let mut n = 0u64;
                for byte in value {
                    n = (n << 8) | u64::from(*byte);
                }
                out.insert(key, n);
            } else if kind == 10 {
                let start = strings + be32(b, at)? as usize;
                ensure!(
                    b.get(start..data)
                        .is_some_and(|v| v.iter().take(4097).any(|b| *b == 0)),
                    "UTF string bounds"
                );
            } else if kind == 11 {
                let off = be32(b, at)? as usize;
                let len = be32(b, at + 4)? as usize;
                ensure!(
                    off <= end - data && len <= end - data - off,
                    "UTF data bounds"
                );
            }
        } else if kind <= 7 {
            out.insert(key, 0);
        }
        ensure!(schema <= rows, "UTF schema bounds");
    }
    Ok((name, out))
}
pub fn extract(path: &Path, stage: &Path, key: Option<u64>, limit: u64) -> Result<Vec<Stream>> {
    let mut input = File::open(path)?;
    let size = input.metadata()?.len();
    let mut pos = 0u64;
    let mut total = 0u64;
    let mut streams: BTreeMap<([u8; 4], u8), Pending> = BTreeMap::new();
    let masks = key.map(get_mask);
    let mut crid = false;
    let mut declared = std::collections::BTreeSet::new();
    while pos < size {
        let mut h = [0u8; 32];
        input
            .read_exact(&mut h)
            .context("truncated USM chunk header")?;
        let sig: [u8; 4] = h[..4].try_into()?;
        let n = u32::from_be_bytes(h[4..8].try_into()?) as u64 + 8;
        let header = u16::from_be_bytes(h[8..10].try_into()?) as u64;
        let padding = u16::from_be_bytes(h[10..12].try_into()?) as u64;
        ensure!(
            n >= 32 && n <= size - pos && header >= 24 && header + padding <= n - 8,
            "invalid USM chunk bounds"
        );
        ensure!(
            matches!(&sig, b"CRID" | b"@SFV" | b"@SFA" | b"@ALP"),
            "unsupported USM stream type"
        );
        ensure!(pos != 0 || &sig == b"CRID", "missing USM CRID");
        let kind = h[15] & 3;
        let len = n - 8 - header - padding;
        ensure!(
            len <= limit && (kind == 0 || len <= 16 << 20),
            "USM chunk budget"
        );
        input.seek(SeekFrom::Start(pos + 8 + header))?;
        let mut payload = vec![0; len as usize];
        input.read_exact(&mut payload)?;
        if &sig == b"CRID" {
            ensure!(!crid && kind == 1, "duplicate/invalid CRID");
            let (name, _) = table(&payload)?;
            ensure!(
                name == "CRIUSF_DIR_STREAM",
                "unexpected CRID directory table"
            );
            let count = be32(&payload, 28)? as usize;
            ensure!(count <= 4, "USM directory stream count");
            for index in 0..count {
                let (_, row) = table_row(&payload, index)?;
                if let Some(id) = row.get("stmid").copied().filter(|n| *n != 0) {
                    let signature = u32::try_from(id)?.to_be_bytes();
                    let channel = u8::try_from(*row.get("chno").context("USM directory channel")?)?;
                    ensure!(
                        matches!(&signature, b"@SFV" | b"@SFA" | b"@ALP")
                            && declared.insert((signature, channel)),
                        "unsupported/duplicate USM directory stream"
                    );
                }
            }
            crid = true;
        } else {
            let id = (sig, h[12]);
            if !streams.contains_key(&id) {
                ensure!(streams.len() < 3, "unsupported USM track count");
                let path = stage.join(format!(
                    "usm-{}-{}.stream",
                    std::str::from_utf8(&sig[1..])?,
                    h[12]
                ));
                streams.insert(
                    id,
                    Pending {
                        file: File::create(&path)?,
                        stream: Stream {
                            signature: sig,
                            channel: h[12],
                            path,
                            video: None,
                            audio_codec: None,
                            chunks: 0,
                        },
                        ended: false,
                        header: false,
                    },
                );
            }
            let s = streams.get_mut(&id).unwrap();
            ensure!(!s.ended, "USM data after stream end");
            match kind {
                0 => {
                    ensure!(s.header, "USM payload before header");
                    total = total.checked_add(len).context("USM size overflow")?;
                    ensure!(total <= limit, "USM expansion limit");
                    if let Some((video, audio)) = &masks {
                        if &sig == b"@SFA" {
                            if s.stream.audio_codec == Some(2) {
                                mask_audio(&mut payload, audio);
                            }
                        } else {
                            mask_video(&mut payload, video);
                        }
                    }
                    s.file.write_all(&payload)?;
                    s.stream.chunks += 1;
                }
                1 => {
                    ensure!(!s.header, "duplicate USM stream header");
                    let (name, t) = table(&payload)?;
                    if &sig == b"@SFA" {
                        ensure!(name == "AUDIO_HDRINFO", "USM audio header table");
                        s.stream.audio_codec = Some(t.get("audio_codec").copied().unwrap_or(2));
                        ensure!(
                            matches!(s.stream.audio_codec, Some(2 | 4)),
                            "unsupported USM audio codec"
                        );
                    } else {
                        ensure!(name == "VIDEO_HDRINFO", "USM video header table");
                        s.stream.video = Some(VideoInfo::from_table(&t)?);
                    }
                    s.header = true;
                }
                2 => {
                    if payload.starts_with(b"#CONTENTS END") {
                        ensure!(s.stream.chunks > 0, "empty USM stream");
                        s.ended = true;
                    } else {
                        ensure!(
                            payload.starts_with(b"#HEADER END")
                                || payload.starts_with(b"#METADATA END"),
                            "unknown USM marker"
                        );
                    }
                }
                3 => {
                    table(&payload)?;
                }
                _ => unreachable!(),
            }
        }
        pos += n;
        input.seek(SeekFrom::Start(pos))?;
    }
    ensure!(
        declared.is_empty() || declared == streams.keys().copied().collect(),
        "USM directory stream missing or unexpected"
    );
    ensure!(
        crid && streams.values().all(|s| s.header && s.ended),
        "incomplete USM streams"
    );
    ensure!(
        streams.keys().filter(|(s, _)| s == b"@SFV").count() == 1
            && streams.keys().filter(|(s, _)| s == b"@SFA").count() <= 1
            && streams.keys().filter(|(s, _)| s == b"@ALP").count() <= 1,
        "unsupported USM tracks"
    );
    let mut result = vec![];
    for (_, mut s) in streams {
        s.file.flush()?;
        drop(s.file);
        result.push(s.stream);
    }
    Ok(result)
}

// Mask routines adapted from cridecoder 0.3.5 (MIT), src/usm/extractor.rs.
// Copyright notice and license retained in third_party/licenses/cridecoder-0.3.5.
const MASK_LEN: usize = 32;
type VideoMask = ([u8; MASK_LEN], [u8; MASK_LEN]);
type AudioMask = [u8; MASK_LEN];
fn get_mask(key: u64) -> (VideoMask, AudioMask) {
    let key1 = (key & 0xFFFFFFFF) as u32;
    let key2 = ((key >> 32) & 0xFFFFFFFF) as u32;

    let mut t = [0u8; MASK_LEN];
    t[0x00] = (key1 & 0xFF) as u8;
    t[0x01] = ((key1 >> 8) & 0xFF) as u8;
    t[0x02] = ((key1 >> 16) & 0xFF) as u8;
    t[0x03] = (((key1 >> 24) & 0xFF) as u8).wrapping_sub(0x34);
    t[0x04] = ((key2 & 0xF) as u8).wrapping_add(0xF9);
    t[0x05] = ((key2 >> 8) & 0xFF) as u8 ^ 0x13;
    t[0x06] = (((key2 >> 16) & 0xFF) as u8).wrapping_add(0x61);
    t[0x07] = t[0x00] ^ 0xFF;
    t[0x08] = (t[0x02] as u16 + t[0x01] as u16) as u8;
    t[0x09] = (t[0x01] as i16 - t[0x07] as i16) as u8;
    t[0x0A] = t[0x02] ^ 0xFF;
    t[0x0B] = t[0x01] ^ 0xFF;
    t[0x0C] = (t[0x0B] as u16 + t[0x09] as u16) as u8;
    t[0x0D] = (t[0x08] as i16 - t[0x03] as i16) as u8;
    t[0x0E] = t[0x0D] ^ 0xFF;
    t[0x0F] = (t[0x0A] as i16 - t[0x0B] as i16) as u8;
    t[0x10] = (t[0x08] as i16 - t[0x0F] as i16) as u8;
    t[0x11] = t[0x10] ^ t[0x07];
    t[0x12] = t[0x0F] ^ 0xFF;
    t[0x13] = t[0x03] ^ 0x10;
    t[0x14] = (t[0x04] as i16 - 0x32) as u8;
    t[0x15] = (t[0x05] as u16 + 0xED) as u8;
    t[0x16] = t[0x06] ^ 0xF3;
    t[0x17] = (t[0x13] as i16 - t[0x0F] as i16) as u8;
    t[0x18] = (t[0x15] as u16 + t[0x07] as u16) as u8;
    t[0x19] = (0x21i16 - t[0x13] as i16) as u8;
    t[0x1A] = t[0x14] ^ t[0x17];
    t[0x1B] = (t[0x16] as u16 + t[0x16] as u16) as u8;
    t[0x1C] = (t[0x17] as u16 + 0x44) as u8;
    t[0x1D] = (t[0x03] as u16 + t[0x04] as u16) as u8;
    t[0x1E] = (t[0x05] as i16 - t[0x16] as i16) as u8;
    t[0x1F] = t[0x1D] ^ t[0x13];

    let t2 = b"URUC";
    let mut vmask1 = [0u8; MASK_LEN];
    let mut vmask2 = [0u8; MASK_LEN];
    let mut amask = [0u8; MASK_LEN];

    for (i, &ti) in t.iter().enumerate() {
        vmask1[i] = ti;
        vmask2[i] = ti ^ 0xFF;
        if i & 1 != 0 {
            amask[i] = t2[(i >> 1) & 3];
        } else {
            amask[i] = ti ^ 0xFF;
        }
    }

    ((vmask1, vmask2), amask)
}

/// De-mask video content **in place**.
///
/// The bulk (second) pass is written as a 32-byte (mask-width) chunked XOR so
/// LLVM auto-vectorizes it to SSE2/NEON/AVX with no platform-specific
/// intrinsics. The per-lane mask recurrence is preserved exactly: the 32 lanes
/// are independent, so each 32-byte row does `row ^= mask; mask = row ^ vmask1`
/// as two vector ops with `mask` carried in a register across rows. The first
/// pass (256 B, negligible) stays scalar to avoid an aliasing split. Output is
/// bit-identical to the original scalar version (locked by `mask_golden`).
fn mask_video(buf: &mut [u8], vmask: &VideoMask) {
    let len = buf.len();
    if len.saturating_sub(0x40) < 0x200 {
        return;
    }
    let vm1 = vmask.1;
    let mut mask = vmask.1;

    // Second pass: original i in 0x100..size -> buf[0x140..len]. 0x100 is
    // 32-aligned, so row position j maps to mask lane j.
    {
        let (chunks, remainder) = buf[0x140..len].as_chunks_mut::<MASK_LEN>();
        for row in chunks {
            for j in 0..MASK_LEN {
                row[j] ^= mask[j];
                mask[j] = row[j] ^ vm1[j];
            }
        }
        for (j, b) in remainder.iter_mut().enumerate() {
            *b ^= mask[j];
            mask[j] = *b ^ vm1[j];
        }
    }

    // First pass: i in 0..0x100 reads the now-decoded buf[0x140..0x240].
    let mut mask = vmask.0;
    for i in 0..0x100 {
        let v = buf[0x140 + i];
        let l = i & 0x1F;
        mask[l] ^= v;
        buf[0x40 + i] ^= mask[l];
    }
}

/// De-mask audio content **in place** (simple repeating 32-byte XOR). Chunked
/// to the mask width so LLVM auto-vectorizes. Bit-identical to the original.
fn mask_audio(buf: &mut [u8], amask: &AudioMask) {
    let Some(region) = buf.get_mut(0x140..) else {
        return;
    };
    let (chunks, remainder) = region.as_chunks_mut::<MASK_LEN>();
    for row in chunks {
        for j in 0..MASK_LEN {
            row[j] ^= amask[j];
        }
    }
    for (j, b) in remainder.iter_mut().enumerate() {
        *b ^= amask[j];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn utf(name: &str, fields: &[(&str, u32)]) -> Vec<u8> {
        let mut strings = vec![0];
        let name_at = strings.len();
        strings.extend(name.as_bytes());
        strings.push(0);
        let mut schema = vec![];
        for (key, value) in fields {
            let at = strings.len();
            strings.extend(key.as_bytes());
            strings.push(0);
            schema.push(0x34);
            schema.extend((at as u32).to_be_bytes());
            schema.extend(value.to_be_bytes());
        }
        let rows = 32 + schema.len();
        let data = rows + strings.len();
        let mut b = vec![0; 32];
        b[..4].copy_from_slice(b"@UTF");
        b[4..8].copy_from_slice(&((data - 8) as u32).to_be_bytes());
        b[8..12].copy_from_slice(&((rows - 8) as u32).to_be_bytes());
        b[12..16].copy_from_slice(&((rows - 8) as u32).to_be_bytes());
        b[16..20].copy_from_slice(&((data - 8) as u32).to_be_bytes());
        b[20..24].copy_from_slice(&(name_at as u32).to_be_bytes());
        b[24..26].copy_from_slice(&(fields.len() as u16).to_be_bytes());
        b[28..32].copy_from_slice(&1u32.to_be_bytes());
        b.extend(schema);
        b.extend(strings);
        b
    }
    fn chunk(sig: &[u8; 4], kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0; 32];
        b[..4].copy_from_slice(sig);
        b[4..8].copy_from_slice(&((payload.len() + 24) as u32).to_be_bytes());
        b[8..10].copy_from_slice(&24u16.to_be_bytes());
        b[15] = kind;
        b.extend(payload);
        b
    }
    fn header() -> Vec<u8> {
        utf(
            "VIDEO_HDRINFO",
            &[
                ("width", 8),
                ("height", 8),
                ("disp_width", 7),
                ("disp_height", 7),
                ("total_frames", 2),
                ("framerate_n", 30000),
                ("framerate_d", 1001),
                ("mpeg_codec", 1),
            ],
        )
    }
    fn fixture(audio_first: bool) -> Vec<u8> {
        let mut b = chunk(b"CRID", 1, &utf("CRIUSF_DIR_STREAM", &[]));
        let v = chunk(b"@SFV", 1, &header());
        let a = chunk(b"@SFA", 1, &utf("AUDIO_HDRINFO", &[("audio_codec", 2)]));
        if audio_first {
            b.extend(a);
            b.extend(v);
        } else {
            b.extend(v);
            b.extend(a);
        }
        b.extend(chunk(b"@SFV", 0, b"video-one"));
        b.extend(chunk(b"@SFA", 0, b"audio"));
        b.extend(chunk(b"@SFA", 2, b"#CONTENTS END"));
        b.extend(chunk(
            b"@SFV",
            3,
            &utf("VIDEO_SEEKINFO", &[("ofs_frmid", 0)]),
        ));
        b.extend(chunk(b"@SFV", 0, b"video-two"));
        b.extend(chunk(b"@SFV", 2, b"#CONTENTS END"));
        b
    }
    #[test]
    fn order_metadata_and_per_stream_ends() {
        for order in [false, true] {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("a.usm");
            std::fs::write(&p, fixture(order)).unwrap();
            let streams = extract(&p, d.path(), None, 10000).unwrap();
            let v = streams.iter().find(|s| &s.signature == b"@SFV").unwrap();
            assert_eq!(std::fs::read(&v.path).unwrap(), b"video-onevideo-two");
            assert_eq!(v.chunks, 2);
            assert_eq!(v.video.as_ref().unwrap().fps(), "30000/1001");
        }
    }
    #[test]
    fn truncation_unknown_stream_missing_end_and_bounds() {
        let good = fixture(false);
        for bad in [
            good[..good.len() - 1].to_vec(),
            good[..good.len() - 44].to_vec(),
            [good.clone(), chunk(b"@XXX", 0, b"x")].concat(),
            [good.clone(), chunk(b"@SFV", 0, b"late")].concat(),
        ] {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("a");
            std::fs::write(&p, bad).unwrap();
            assert!(extract(&p, d.path(), None, 10000).is_err());
        }
        let mut bad = header();
        bad[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(table(&bad).is_err());
    }
    #[test]
    fn mutated_metadata_never_panics() {
        let good = header();
        for n in 0..good.len() {
            let mut b = good.clone();
            b[n] = 255;
            assert!(std::panic::catch_unwind(|| table(&b)).is_ok());
        }
    }
    #[test]
    fn rational_timebase_and_dimensions() {
        for (n, d) in [(30, 1), (24000, 1001), (30000, 1001)] {
            let (_, mut fields) = table(&header()).unwrap();
            fields.insert("framerate_n".into(), n);
            fields.insert("framerate_d".into(), d);
            let v = VideoInfo::from_table(&fields).unwrap();
            assert!((v.duration() - 2.0 * d as f64 / n as f64).abs() < 1e-12);
            fields.insert("disp_width".into(), 99);
            assert!(VideoInfo::from_table(&fields).is_err());
        }
    }
}

#[cfg(test)]
mod mask_boundary_tests {
    use super::*;
    #[test]
    fn short_video_packets_are_unmasked() {
        let masks = get_mask(8594927479);
        for len in [0, 63, 64, 319, 512, 559, 563, 575] {
            let mut b: Vec<_> = (0..len).map(|i| (i * 13) as u8).collect();
            let old = b.clone();
            mask_video(&mut b, &masks.0);
            assert_eq!(b, old);
        }
        let mut b = vec![7; 576];
        mask_video(&mut b, &masks.0);
        assert_ne!(b, vec![7; 576]);
    }
}

#[cfg(test)]
mod directory_tests {
    use super::*;
    #[test]
    fn stream_directory_requires_declared_mask() {
        // Synthetic directory table declares @ALP channel 0 but supplies no stream.
        let names = b"\0CRIUSF_DIR_STREAM\0stmid\0chno\0";
        let mut table = vec![0; 50];
        table[..4].copy_from_slice(b"@UTF");
        let end = 50 + names.len();
        table[4..8].copy_from_slice(&((end - 8) as u32).to_be_bytes());
        for at in [8, 12] {
            table[at..at + 4].copy_from_slice(&42u32.to_be_bytes());
        }
        table[16..20].copy_from_slice(&((end - 8) as u32).to_be_bytes());
        table[20..24].copy_from_slice(&1u32.to_be_bytes());
        table[24..26].copy_from_slice(&2u16.to_be_bytes());
        table[28..32].copy_from_slice(&1u32.to_be_bytes());
        table[32] = 0x34;
        table[33..37].copy_from_slice(&19u32.to_be_bytes());
        table[37..41].copy_from_slice(b"@ALP");
        table[41] = 0x34;
        table[42..46].copy_from_slice(&25u32.to_be_bytes());
        table.extend(names);
        let mut bytes = vec![0; 32];
        bytes[..4].copy_from_slice(b"CRID");
        bytes[4..8].copy_from_slice(&((24 + table.len()) as u32).to_be_bytes());
        bytes[8..10].copy_from_slice(&24u16.to_be_bytes());
        bytes[15] = 1;
        bytes.extend(table);
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        std::fs::write(&p, bytes).unwrap();
        let error = extract(&p, d.path(), None, 10000).unwrap_err();
        assert!(
            error.to_string().contains("directory stream missing"),
            "{error}"
        );
    }
}
