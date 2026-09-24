//! Strict, sample-exact ADX input normalization for FFmpeg's 128-block packets.
use anyhow::{Result, ensure};
use serde::Serialize;
#[derive(Debug, Serialize)]
pub struct Info {
    pub channels: u8,
    pub rate: u32,
    pub samples: u32,
    pub padded_blocks: usize,
}
pub fn normalize(raw: &[u8], limit: u64) -> Result<(Vec<u8>, Info)> {
    ensure!(
        raw.len() >= 20 && raw[..2] == [0x80, 0] && raw[4..7] == [3, 18, 4],
        "unsupported ADX header"
    );
    let header = u16::from_be_bytes(raw[2..4].try_into()?) as usize + 4;
    let channels = raw[7];
    let rate = u32::from_be_bytes(raw[8..12].try_into()?);
    let samples = u32::from_be_bytes(raw[12..16].try_into()?);
    ensure!(
        header >= 20
            && header <= raw.len()
            && &raw[header - 6..header] == b"(c)CRI"
            && (1..=2).contains(&channels)
            && (8000..=192000).contains(&rate)
            && samples > 0
            && matches!(raw[18], 3 | 4)
            && raw[19] == 0,
        "invalid/encrypted ADX header"
    );
    ensure!(
        samples as u64 * channels as u64 * 2 <= limit,
        "ADX PCM budget"
    );
    let blocks = (samples as usize).div_ceil(32);
    let end = header + blocks * 18 * channels as usize;
    ensure!(end <= raw.len(), "ADX sample count exceeds payload");
    let tail = &raw[end..];
    ensure!(
        tail.len() >= 4
            && tail[..2] == [0x80, 1]
            && u16::from_be_bytes(tail[2..4].try_into()?) as usize + 4 == tail.len()
            && tail[4..].iter().all(|v| *v == 0),
        "invalid ADX footer or sample count"
    );
    let padded_blocks = (128 - blocks % 128) % 128;
    let mut data = raw[..end].to_vec();
    data.resize(end + padded_blocks * 18 * channels as usize, 0);
    Ok((
        data,
        Info {
            channels,
            rate,
            samples,
            padded_blocks,
        },
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn sample(channels: u8, blocks: usize) -> Vec<u8> {
        let mut v = vec![0; 36 + blocks * 18 * channels as usize];
        v[..8].copy_from_slice(&[0x80, 0, 0, 32, 3, 18, 4, channels]);
        v[8..12].copy_from_slice(&48000u32.to_be_bytes());
        v[12..16].copy_from_slice(&((blocks * 32 - 1) as u32).to_be_bytes());
        v[18] = 4;
        v[30..36].copy_from_slice(b"(c)CRI");
        v.extend([0x80, 1, 0, 0]);
        v
    }
    #[test]
    fn all_packet_tails_and_channels() {
        for channels in [1, 2] {
            for blocks in 1..=128 {
                let (data, i) = normalize(&sample(channels, blocks), 1 << 20).unwrap();
                assert_eq!((data.len() - 36) / (18 * channels as usize), 128);
                assert_eq!(i.samples as usize, blocks * 32 - 1);
            }
        }
    }
    #[test]
    fn refuses_missing_corrupt_footer_truncated_samples_and_key() {
        let raw = sample(1, 2);
        for mut bad in [
            raw[..raw.len() - 4].to_vec(),
            raw.clone(),
            raw.clone(),
            raw.clone(),
        ] {
            if bad.len() == raw.len() {
                bad[raw.len() - 1] = 1;
            }
            assert!(normalize(&bad, 1 << 20).is_err());
            bad[19] = 8;
            assert!(normalize(&bad, 1 << 20).is_err());
        }
        let mut bad = raw;
        bad[12..16].copy_from_slice(&10000u32.to_be_bytes());
        assert!(normalize(&bad, 1 << 20).is_err());
    }
}
