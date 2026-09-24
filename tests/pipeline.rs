mod support;
use moenotes_assets::{
    catalog::Catalog,
    config::Config,
    crypto,
    worker::{Input, Job},
};
use support::*;

#[test]
fn synthetic_worker_export_and_crc() {
    let dir = tempfile::tempdir().unwrap();
    let (cat, encrypted) = fixture();
    let c = Catalog::parse(&cat).unwrap();
    let target = c.target(KEY).unwrap().clone();
    let location = c.closure(KEY).unwrap().remove(0);
    let mut plain = encrypted;
    crypto::decrypt(&mut plain, "fixture.bundle", 0).unwrap();
    let path = dir.path().join("fixture.bundle");
    std::fs::write(&path, &plain).unwrap();
    assert_eq!(
        crypto::bundle_crc(&path, 1 << 20).unwrap(),
        location.options.as_ref().unwrap().crc
    );
    let job = Job {
        config: Config {
            cdn_root: "https://cdn.invalid".into(),
            ..Default::default()
        },
        target,
        inputs: vec![Input { location, path }],
        archive: false,
        output: dir.path().join("out"),
    };
    let files = moenotes_assets::worker::run(&job).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        std::fs::read(job.output.join(&files[0].name)).unwrap(),
        BODY
    );
}

#[test]
fn dependency_completion_order_does_not_change_export_ids() {
    let dir = tempfile::tempdir().unwrap();
    let (cat, _) = fixture();
    let c = Catalog::parse(&cat).unwrap();
    let target = c.target(KEY).unwrap().clone();
    let base = c.closure(KEY).unwrap().remove(0);
    let mut inputs = vec![];
    for (index, name, marker) in [(1, "CAB-first", b"alpha"), (2, "CAB-other", b"bravo")] {
        let mut payload = serialized();
        let mut at = 0;
        while let Some(pos) = payload[at..].windows(7).position(|v| v == b"fixture") {
            let start = at + pos;
            payload[start..start + 5].copy_from_slice(marker);
            at = start + 7;
        }
        // Keep the logical container path unchanged; only the object labels differ.
        for start in 0..payload.len() - INTERNAL.len() {
            if payload[start..start + INTERNAL.len()].starts_with(b"Assets/") {
                payload[start..start + INTERNAL.len()].copy_from_slice(INTERNAL.as_bytes());
                break;
            }
        }
        let (bytes, crc) = bundle_named(payload, name);
        let path = dir.path().join(format!("{index}.bundle"));
        std::fs::write(&path, &bytes).unwrap();
        let mut location = base.clone();
        location.id = index;
        let o = location.options.as_mut().unwrap();
        o.size = bytes.len() as u64;
        o.crc = crc;
        inputs.push(Input { location, path });
    }
    let mut job = Job {
        config: Config {
            cdn_root: "https://cdn.invalid".into(),
            ..Default::default()
        },
        target,
        inputs,
        archive: false,
        output: dir.path().join("first"),
    };
    let first = moenotes_assets::worker::run(&job).unwrap();
    assert_eq!(first.len(), 2);
    job.inputs.reverse();
    job.output = dir.path().join("second");
    let second = moenotes_assets::worker::run(&job).unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second).unwrap()
    );
}

#[test]
fn catalog_and_container_corruption_rejected() {
    let (cat, _) = fixture();
    for n in [0, 4, 16, 31, cat.len() / 2] {
        assert!(Catalog::parse(&cat[..n]).is_err());
    }
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bad.bundle");
    std::fs::write(&p, [0; 128]).unwrap();
    assert!(crypto::bundle_crc(&p, 1 << 20).is_err());
}

#[test]
fn texture_sprite_aliases_only_when_same_source() {
    let (cat, _) = fixture();
    let mut c = Catalog::parse(&cat).unwrap();
    let mut second = c.target(KEY).unwrap().clone();
    second.id += 1;
    second.resource_type = "UnityEngine.Sprite".into();
    c.locations.insert(second.id, second.clone());
    c.keys.get_mut(KEY).unwrap().push(second.id);
    assert!(c.target(KEY).is_ok());
    c.locations.get_mut(&second.id).unwrap().internal.push('x');
    assert!(c.target(KEY).is_err());
}

#[test]
fn worker_crc_and_configuration_limits_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (cat, mut raw) = fixture();
    let c = Catalog::parse(&cat).unwrap();
    let mut location = c.closure(KEY).unwrap().remove(0);
    location.options.as_mut().unwrap().crc ^= 1;
    crypto::decrypt(&mut raw, "fixture.bundle", 0).unwrap();
    let p = dir.path().join("a.bundle");
    std::fs::write(&p, raw).unwrap();
    let job = Job {
        config: Config {
            cdn_root: "https://cdn.invalid".into(),
            ..Default::default()
        },
        target: c.target(KEY).unwrap().clone(),
        inputs: vec![Input { location, path: p }],
        archive: false,
        output: dir.path().join("out"),
    };
    assert!(
        moenotes_assets::worker::run(&job)
            .unwrap_err()
            .to_string()
            .contains("CRC")
    );
    let mut c = job.config;
    c.temp_bytes = 1 << 20;
    assert!(c.validate().is_err());
    c.temp_bytes = 20 << 30;
    c.workers = 0;
    assert!(c.validate().is_err());
}

#[test]
fn generated_encrypted_hca_to_aac_and_wrong_key() {
    use std::io::Cursor;
    let dir = tempfile::tempdir().unwrap();
    let samples: Vec<f32> = (0..4800).map(|i| ((i as f32) * 0.08).sin() * 0.2).collect();
    let mut hca = Cursor::new(Vec::new());
    cridecoder::HcaEncoder::new(
        cridecoder::HcaEncoderConfig::new(48000, 1).with_encryption(8594927479),
    )
    .unwrap()
    .encode(&samples, &mut hca)
    .unwrap();
    let mut builder = cridecoder::AcbBuilder::new();
    builder.add_track(cridecoder::TrackInput::new(
        "synthetic-tone",
        0,
        hca.into_inner(),
    ));
    let mut bank = Cursor::new(Vec::new());
    builder.build(&mut bank, None).unwrap();
    let path = dir.path().join("bank.acb");
    std::fs::write(&path, bank.into_inner()).unwrap();
    let location = moenotes_assets::catalog::Location {
        id: 1,
        key: "sound/test".into(),
        internal: "https://cdn.invalid/asset/Android/test".into(),
        provider: moenotes_assets::catalog::CRI.into(),
        resource_type: "CriWare.Assets.CriAtomAcbAsset".into(),
        dependencies: vec![],
        options: None,
    };
    let job = Job {
        config: Config {
            cdn_root: "https://cdn.invalid".into(),
            data_dir: dir.path().into(),
            ..Default::default()
        },
        target: location.clone(),
        inputs: vec![Input { location, path }],
        archive: false,
        output: dir.path().join("out"),
    };
    // Exercise the real executable: its worker creates media-exec child processes.
    let request = dir.path().join("job.json");
    std::fs::write(&request, serde_json::to_vec(&job).unwrap()).unwrap();
    assert!(
        std::process::Command::new(env!("CARGO_BIN_EXE_moenotes-assets"))
            .arg("worker")
            .arg(&request)
            .status()
            .unwrap()
            .success()
    );
    let result: moenotes_assets::worker::WorkerResult =
        serde_json::from_slice(&std::fs::read(request.with_extension("result.json")).unwrap())
            .unwrap();
    assert!(result.error.is_none(), "{:?}", result.error);
    assert_eq!(result.files[0].media_type, "audio/mp4");
    let mut failing: Job = serde_json::from_slice(&serde_json::to_vec(&job).unwrap()).unwrap();
    failing.config.ffmpeg = "/nonexistent/private/ffmpeg".into();
    failing.output = dir.path().join("failing-media");
    std::fs::write(&request, serde_json::to_vec(&failing).unwrap()).unwrap();
    assert!(
        std::process::Command::new(env!("CARGO_BIN_EXE_moenotes-assets"))
            .arg("worker")
            .arg(&request)
            .status()
            .unwrap()
            .success()
    );
    let failure: moenotes_assets::worker::WorkerResult =
        serde_json::from_slice(&std::fs::read(request.with_extension("result.json")).unwrap())
            .unwrap();
    let error = failure.error.unwrap();
    assert!(error.starts_with("media_encode_failed"), "{error}");
    assert!(error.contains("stage=encode") && error.contains("diagnostic_id="));
    assert!(!error.contains("/nonexistent") && !error.contains(&dir.path().display().to_string()));
    assert_eq!(
        std::fs::read_dir(dir.path().join("diagnostics"))
            .unwrap()
            .count(),
        1
    );
    let mut wrong = job;
    wrong.config.cri_key += 1;
    wrong.output = dir.path().join("wrong");
    std::fs::write(&request, serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(
        std::process::Command::new(env!("CARGO_BIN_EXE_moenotes-assets"))
            .arg("worker")
            .arg(&request)
            .status()
            .unwrap()
            .success()
    );
    let result: moenotes_assets::worker::WorkerResult =
        serde_json::from_slice(&std::fs::read(request.with_extension("result.json")).unwrap())
            .unwrap();
    assert!(result.error.is_some());
}

#[test]
fn mutated_catalog_never_panics() {
    let (c, _) = fixture();
    for pos in (0..c.len()).step_by(3) {
        let mut b = c.clone();
        b[pos] = 255;
        assert!(
            std::panic::catch_unwind(|| Catalog::parse(&b)).is_ok(),
            "{pos}"
        );
    }
}

#[test]
fn explicit_location_type_and_archive_contracts() {
    use moenotes_assets::catalog::Selector;
    let (cat, mut raw) = fixture();
    let mut c = Catalog::parse(&cat).unwrap();
    let first = c.target(KEY).unwrap().clone();
    let mut second = first.clone();
    second.id += 1;
    second.internal = "Assets/other".into();
    second.resource_type = "UnityEngine.Sprite".into();
    c.locations.insert(second.id, second.clone());
    c.keys.get_mut(KEY).unwrap().push(second.id);
    assert!(c.target(KEY).is_err());
    assert_eq!(
        c.resolve(
            KEY,
            &Selector {
                expected_type: Some("UnityEngine.Sprite".into()),
                location_id: None
            }
        )
        .unwrap()
        .id,
        second.id
    );
    assert_eq!(
        c.resolve(
            KEY,
            &Selector {
                expected_type: None,
                location_id: Some(first.id)
            }
        )
        .unwrap()
        .id,
        first.id
    );
    assert!(
        c.resolve(
            KEY,
            &Selector {
                expected_type: Some("UnityEngine.Sprite".into()),
                location_id: Some(first.id)
            }
        )
        .is_err()
    );
    let d = tempfile::tempdir().unwrap();
    crypto::decrypt(&mut raw, "fixture.bundle", 0).unwrap();
    let path = d.path().join("input");
    std::fs::write(&path, &raw).unwrap();
    let job = Job {
        config: Config {
            cdn_root: "https://cdn.invalid".into(),
            ..Default::default()
        },
        target: first.clone(),
        inputs: vec![Input {
            location: c.closure_from(first.id).unwrap().remove(0),
            path,
        }],
        output: d.path().join("out"),
        archive: true,
    };
    let files = moenotes_assets::worker::run(&job).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(std::fs::read(job.output.join(&files[0].name)).unwrap(), raw);
    assert_eq!(files[0].media_type, "application/vnd.unity");
}

fn usm_table(name: &str, fields: &[(&str, u32)]) -> Vec<u8> {
    let mut strings = vec![0];
    let name_at = strings.len();
    strings.extend(name.as_bytes());
    strings.push(0);
    let mut columns = vec![];
    for (key, value) in fields {
        let at = strings.len();
        strings.extend(key.as_bytes());
        strings.push(0);
        columns.push(0x34);
        columns.extend((at as u32).to_be_bytes());
        columns.extend(value.to_be_bytes());
    }
    let rows = 32 + columns.len();
    let end = rows + strings.len();
    let mut b = vec![0; 32];
    b[..4].copy_from_slice(b"@UTF");
    for (offset, value) in [
        (4, end - 8),
        (8, rows - 8),
        (12, rows - 8),
        (16, end - 8),
        (20, name_at),
        (28, 1),
    ] {
        b[offset..offset + 4].copy_from_slice(&(value as u32).to_be_bytes());
    }
    b[24..26].copy_from_slice(&(fields.len() as u16).to_be_bytes());
    b.extend(columns);
    b.extend(strings);
    b
}
fn usm_chunk(signature: &[u8; 4], kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![0; 32];
    b[..4].copy_from_slice(signature);
    b[4..8].copy_from_slice(&((payload.len() + 24) as u32).to_be_bytes());
    b[8..10].copy_from_slice(&24u16.to_be_bytes());
    b[15] = kind;
    b.extend(payload);
    b
}
#[test]
fn synthetic_usm_timing_alpha_pixels_and_missing_frames() {
    use std::process::Command;
    let d = tempfile::tempdir().unwrap();
    let video = d.path().join("source.m2v");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=32x32:rate=25",
                "-frames:v",
                "5",
                "-c:v",
                "mpeg1video",
                "-bf",
                "2",
                "-threads",
                "1"
            ])
            .arg(&video)
            .status()
            .unwrap()
            .success()
    );
    let payload = std::fs::read(&video).unwrap();
    for (i, (n, den, frames)) in [(30, 1, 5), (24000, 1001, 5), (30000, 1001, 5), (30, 1, 6)]
        .into_iter()
        .enumerate()
    {
        let mut data = usm_chunk(b"CRID", 1, &usm_table("CRIUSF_DIR_STREAM", &[]));
        for sig in [b"@SFV", b"@ALP"] {
            let h = usm_table(
                "VIDEO_HDRINFO",
                &[
                    ("width", 32),
                    ("height", 32),
                    ("disp_width", 31),
                    ("disp_height", 31),
                    ("total_frames", frames),
                    ("framerate_n", n),
                    ("framerate_d", den),
                    ("mpeg_codec", 1),
                ],
            );
            data.extend(usm_chunk(sig, 1, &h));
            data.extend(usm_chunk(sig, 0, &payload));
            data.extend(usm_chunk(sig, 2, b"#CONTENTS END"));
        }
        let input = d.path().join(format!("{i}.usm"));
        std::fs::write(&input, data).unwrap();
        let location = moenotes_assets::catalog::Location {
            id: 1,
            key: "video/test".into(),
            internal: "test".into(),
            provider: moenotes_assets::catalog::CRI.into(),
            resource_type: "CriWare.Assets.CriManaUsmAsset".into(),
            dependencies: vec![],
            options: None,
        };
        let job = Job {
            config: Config {
                cdn_root: "https://cdn.invalid".into(),
                data_dir: d.path().into(),
                usm_decryption: moenotes_assets::usm::Decryption::Plaintext,
                ffmpeg_threads: 1,
                ..Default::default()
            },
            target: location.clone(),
            inputs: vec![Input {
                location,
                path: input,
            }],
            output: d.path().join(format!("out-{i}")),
            archive: false,
        };
        let request = d.path().join("job.json");
        std::fs::write(&request, serde_json::to_vec(&job).unwrap()).unwrap();
        assert!(
            Command::new(env!("CARGO_BIN_EXE_moenotes-assets"))
                .arg("worker")
                .arg(&request)
                .status()
                .unwrap()
                .success()
        );
        let result: moenotes_assets::worker::WorkerResult =
            serde_json::from_slice(&std::fs::read(request.with_extension("result.json")).unwrap())
                .unwrap();
        if frames == 6 {
            assert!(result.error.unwrap().contains("frame count mismatch"));
            assert!(result.files.is_empty());
            continue;
        }
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.files.len(), 2);
        let mask = result
            .files
            .iter()
            .find(|f| f.metadata["role"] == "alpha-mask")
            .unwrap();
        let raw = |path: &std::path::Path, filter: bool| {
            let mut command = Command::new("ffmpeg");
            command
                .args(["-nostdin", "-v", "error", "-xerror", "-threads", "1", "-i"])
                .arg(path);
            if filter {
                command.args(["-vf", "crop=31:31:0:0:exact=1"]);
            }
            let output = command
                .args(["-pix_fmt", "gray", "-threads", "1", "-f", "rawvideo", "-"])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let source = raw(&video, true);
        let actual = raw(&job.output.join(&mask.name), false);
        assert_eq!(source.len(), 31 * 31 * 5);
        assert_eq!(
            source, actual,
            "lossless alpha pixels must match every source frame"
        );
    }
}
