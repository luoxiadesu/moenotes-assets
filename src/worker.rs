use crate::{
    adx,
    catalog::{CRI, Location},
    config::Config,
    crypto, diagnostics, usm,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    process::Command,
};
use unity_rs_core::{
    image_export::{ImageFormat, ImageRowOrder, write_rgba_image},
    loader::AssetLoadOptions,
    serialized::ContainerMetadataReadLimits,
    source::Region,
    sprite::SpriteReadLimits,
    studio::{Studio, StudioObject},
    texture::TextureReadLimits,
};

pub const PROFILE: &str = "json-png-aac-h264-mask-http-v3";
#[derive(Clone, Serialize, Deserialize)]
pub struct Input {
    pub location: Location,
    pub path: PathBuf,
}
#[derive(Serialize, Deserialize)]
pub struct Job {
    pub config: Config,
    pub target: Location,
    pub inputs: Vec<Input>,
    pub output: PathBuf,
    #[serde(default)]
    pub archive: bool,
    #[serde(default)]
    pub font_reference: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub label: String,
    pub media_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub metadata: Value,
}
#[derive(Serialize, Deserialize)]
pub struct WorkerResult {
    pub files: Vec<Artifact>,
    pub error: Option<String>,
    #[serde(default)]
    pub empty: bool,
}

struct Output<'a> {
    job: &'a Job,
    files: Vec<Artifact>,
    total: u64,
}
impl Output<'_> {
    fn path(&self, ext: &str) -> PathBuf {
        self.job
            .output
            .join(format!("{:05}.{ext}", self.files.len()))
    }
    fn add(&mut self, path: PathBuf, label: String, media: &str, metadata: Value) -> Result<()> {
        let n = path.metadata()?.len();
        self.total = self.total.checked_add(n).context("output overflow")?;
        ensure!(
            self.total <= self.job.config.output_bytes,
            "output budget exceeded"
        );
        ensure!(self.files.len() < 10_000, "output file count limit");
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        let mut f = File::open(&path)?;
        let mut buf = [0; 65536];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        self.files.push(Artifact {
            name: path
                .file_name()
                .context("output basename")?
                .to_string_lossy()
                .into(),
            label,
            media_type: media.into(),
            bytes: n,
            sha256: hex::encode(hasher.finalize()),
            metadata,
        });
        Ok(())
    }
    fn bytes(&mut self, data: &[u8], ext: &str, label: String, media: &str) -> Result<()> {
        ensure!(
            data.len() as u64 <= self.job.config.output_bytes.saturating_sub(self.total),
            "output budget"
        );
        let p = self.path(ext);
        File::create(&p)?.write_all(data)?;
        self.add(p, label, media, Value::Null)
    }
}

pub fn run(job: &Job) -> Result<Vec<Artifact>> {
    job.config.validate()?;
    ensure!(
        !job.inputs.is_empty() || (job.archive && job.font_reference.is_some()),
        "empty worker inputs"
    );
    std::fs::create_dir(&job.output)?;
    let mut output = Output {
        job,
        files: vec![],
        total: 0,
    };
    if job.archive {
        if let Some(reference) = &job.font_reference {
            output.bytes(
                &serde_json::to_vec_pretty(reference)?,
                "json",
                job.target.key.clone(),
                "application/json",
            )?;
            output.files.last_mut().unwrap().metadata = json!({"role":"font-reference","stable_id":"font-reference","conversion":"not converted"});
        }
        for input in &job.inputs {
            ensure!(
                crate::plan::remote(&input.location),
                "archive accepts only HTTP resources"
            );
            let (extension, mime, metadata) = if input.location.provider == CRI {
                let mut magic = [0; 4];
                File::open(&input.path)?.read_exact(&mut magic)?;
                ensure!(
                    matches!(&magic, b"@UTF" | b"CRID" | b"AFS2" | b"CPK "),
                    "unsupported CRI container signature"
                );
                (
                    if &magic == b"CRID" {
                        "usm"
                    } else if &magic == b"@UTF" {
                        "acb"
                    } else if &magic == b"AFS2" {
                        "awb"
                    } else {
                        "cpk"
                    },
                    "application/octet-stream",
                    json!({"role":"cri-container","stable_id":format!("container-{}",input.location.id),"validation":"length and signature; no codec conversion"}),
                )
            } else {
                let crc = crypto::bundle_crc(&input.path, job.config.expanded_bytes)?;
                ensure!(
                    input
                        .location
                        .options
                        .as_ref()
                        .is_none_or(|o| o.crc == 0 || o.crc == crc),
                    "bundle CRC mismatch"
                );
                (
                    "bundle",
                    "application/vnd.unity",
                    json!({"role":"unity-bundle","stable_id":format!("bundle-{}",input.location.id),"crc32":crc,"location_id":input.location.id}),
                )
            };
            let path = output.path(extension);
            std::fs::copy(&input.path, &path)?;
            output.add(path, input.location.key.clone(), mime, metadata)?;
        }
        return Ok(output.files);
    }
    let mut empty_atlases = 0;
    let media = job.target.provider == CRI
        || (job.target.resource_type.starts_with("CriWare.")
            && job.inputs.iter().any(|i| i.location.provider == CRI));
    if media {
        let raw: Vec<_> = job
            .inputs
            .iter()
            .filter(|i| i.location.provider == CRI)
            .collect();
        ensure!(raw.len() == 1, "unsupported or ambiguous CRI dependencies");
        cri(&raw[0].path, &mut output)?;
    } else {
        let mut regions = vec![];
        let mut sum = 0;
        let mut inputs: Vec<_> = job.inputs.iter().collect();
        inputs.sort_by_key(|i| i.location.id);
        ensure!(
            inputs
                .windows(2)
                .all(|w| w[0].location.id != w[1].location.id),
            "duplicate dependency ID"
        );
        for i in inputs {
            ensure!(
                i.location.provider != CRI,
                "unsupported mixed provider target"
            );
            let size = i.path.metadata()?.len();
            sum += size;
            ensure!(sum <= job.config.expanded_bytes, "input set budget");
            let crc = crypto::bundle_crc(&i.path, job.config.expanded_bytes)?;
            if let Some(options) = &i.location.options {
                ensure!(
                    options.crc == 0 || crc == options.crc,
                    "bundle CRC mismatch"
                );
            }
            let region =
                if size <= job.config.memory_threshold && sum <= job.config.memory_threshold {
                    Region::from_bytes(std::fs::read(&i.path)?)
                } else {
                    Region::from_file(&i.path)?
                };
            regions.push((format!("dependency-{}.bundle", i.location.id), region));
        }
        let mut options = AssetLoadOptions::default();
        options.limits.maximum_expanded_bytes = job.config.expanded_bytes;
        options.limits.maximum_single_entry_bytes = job.config.expanded_bytes.min(512 << 20);
        options.limits.maximum_discovered_files = 10_000;
        options.limits.maximum_object_metadata_entries = 1_000_000;
        let studio = Studio::open_regions_with_options(regions, options)?;
        let mut targets = BTreeSet::new();
        for object in studio.objects().filter(|v| v.class_id() == 142) {
            let metadata = object.read_asset_bundle(ContainerMetadataReadLimits::default())?;
            for entry in metadata.container {
                if entry.key == job.target.internal {
                    let resolved = unity_rs_core::scene::resolve_object_reference(
                        studio.collection(),
                        object.file_index(),
                        entry.asset,
                    )?
                    .context("null asset pointer")?;
                    targets.insert((resolved.file_index, resolved.object.path_id));
                }
            }
        }
        ensure!(
            !targets.is_empty(),
            "target InternalId not found in bundle containers"
        );
        for (file, id) in targets {
            let object = studio.object(file, id).context("resolved object missing")?;
            if job.target.resource_type == crate::split_acb::TYPE {
                let (payload, cue_sheet, chunks) =
                    crate::split_acb::read(&studio, object, job.config.expanded_bytes)?;
                let source_sha = crypto::digest(&payload);
                let p = job
                    .output
                    .parent()
                    .context("worker stage")?
                    .join("split.acb");
                std::fs::write(&p, payload)?;
                let before = output.files.len();
                cri(&p, &mut output)?;
                for file in &mut output.files[before..] {
                    file.metadata["split_acb"] = json!({"cue_sheet":cue_sheet,"chunks":chunks,"reconstructed_sha256":source_sha,"assembly":"ordered-textasset-xor5a-v1"});
                }
            } else if object.class_id() == 114 && job.target.resource_type.starts_with("CriWare.") {
                let file = &studio.collection().serialized_files()[object.file_index()].file;
                let payload = crate::embedded::read(file, object, job.config.expanded_bytes)?;
                let p = job
                    .output
                    .parent()
                    .context("worker stage")?
                    .join("embedded-cri");
                std::fs::write(&p, payload)?;
                cri(&p, &mut output)?;
            } else if object.class_id() == 687078895 {
                let atlas = object.read_sprite_atlas(Default::default())?;
                if atlas.packed_sprites.is_empty() {
                    ensure!(
                        atlas.render_data_entries.is_empty()
                            && atlas.packed_sprite_names.is_empty(),
                        "atlas contains unresolved render data"
                    );
                    empty_atlases += 1;
                }
                for reference in atlas.packed_sprites {
                    let resolved = unity_rs_core::scene::resolve_object_reference(
                        studio.collection(),
                        file,
                        reference,
                    )?
                    .context("null atlas sprite")?;
                    unity_object(
                        studio
                            .object(resolved.file_index, resolved.object.path_id)
                            .context("atlas sprite missing")?,
                        &mut output,
                    )?;
                }
            } else {
                unity_object(object, &mut output)?;
            }
        }
    }
    ensure!(
        !output.files.is_empty() || empty_atlases > 0,
        "no supported outputs"
    );
    Ok(output.files)
}

fn unity_object(object: StudioObject<'_>, out: &mut Output<'_>) -> Result<()> {
    let label = object.name().unwrap_or("unnamed").to_owned();
    let stable_id = format!(
        "unity-{}-{}",
        crypto::digest(object.source_path().as_bytes()),
        object.path_id()
    );
    match object.class_id() {
        49 => {
            let raw =
                object.read_text_bytes(out.job.config.expanded_bytes.min(64 << 20) as usize)?;
            let data = decode_text(raw, out.job.config.expanded_bytes.min(64 << 20))?;
            let (ext, mime) = if serde_json::from_slice::<Value>(&data).is_ok() {
                ("json", "application/json")
            } else if data.starts_with(b"#TITLE ") {
                ("sus", "text/plain; charset=utf-8")
            } else if std::str::from_utf8(&data).is_ok() {
                ("txt", "text/plain; charset=utf-8")
            } else {
                ("bin", "application/octet-stream")
            };
            out.bytes(&data, ext, label, mime)?;
            out.files.last_mut().unwrap().metadata = json!({"stable_id":stable_id});
            Ok(())
        }
        28 | 213 => {
            let limits = TextureReadLimits {
                maximum_dimension: 16384,
                maximum_output_bytes: out.job.config.output_bytes.min(512 << 20),
                maximum_decoder_working_bytes: 512 << 20,
                ..Default::default()
            };
            let (image, order) = if object.class_id() == 28 {
                (
                    object.decode_texture_mip(0, limits)?,
                    ImageRowOrder::UnityDecoded,
                )
            } else {
                (
                    object.decode_sprite(SpriteReadLimits::default(), limits)?,
                    ImageRowOrder::Display,
                )
            };
            let p = out.path("png");
            write_rgba_image(
                &image,
                ImageFormat::Png,
                order,
                out.job.config.output_bytes.saturating_sub(out.total),
                &mut File::create(&p)?,
            )?;
            out.add(
                p,
                label,
                "image/png",
                json!({"width":image.width,"height":image.height,"stable_id":stable_id}),
            )
        }
        id => bail!("unsupported Unity class {id}"),
    }
}

fn command(c: &Config, stage: &'static str, args: &[String]) -> Result<()> {
    let mut command = media_command(&c.ffmpeg)?;
    command
        .args([
            "-nostdin",
            "-v",
            "error",
            "-xerror",
            "-y",
            "-threads",
            &c.ffmpeg_threads.to_string(),
        ])
        .args(args);
    diagnostics::capture(c, command, stage, "ffmpeg")?;
    Ok(())
}
pub fn probe(c: &Config, path: &Path) -> Result<Value> {
    let mut command = media_command(&c.ffprobe)?;
    command
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(path);
    let data = diagnostics::capture(c, command, "probe", "ffprobe")?;
    let mut value: Value = serde_json::from_slice(&data).context("media_probe_invalid_json")?;
    if let Some(format) = value.get_mut("format").and_then(Value::as_object_mut) {
        format.remove("filename");
    }
    Ok(value)
}
fn media_command(binary: &str) -> Result<Command> {
    let mut c = Command::new(std::env::current_exe()?);
    c.arg("media-exec").arg(binary);
    Ok(c)
}
fn validate_media(c: &Config, path: &Path, video: bool) -> Result<Value> {
    let p = probe(c, path)?;
    let streams = p["streams"].as_array().context("missing streams")?;
    ensure!(!streams.is_empty(), "empty media");
    let videos = streams
        .iter()
        .filter(|s| s["codec_type"] == "video")
        .count();
    let audios = streams
        .iter()
        .filter(|s| s["codec_type"] == "audio")
        .count();
    ensure!(
        videos == usize::from(video) && if video { audios <= 1 } else { audios == 1 },
        "output track count mismatch"
    );
    for s in streams {
        match s["codec_type"].as_str() {
            Some("video") => ensure!(video && s["codec_name"] == "h264", "wrong video codec"),
            Some("audio") => ensure!(s["codec_name"] == "aac", "wrong audio codec"),
            _ => bail!("unexpected output stream"),
        }
    }
    command(
        c,
        "verify",
        &[
            "-i".into(),
            path.display().to_string(),
            "-map".into(),
            "0".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ],
    )?;
    Ok(p)
}
fn wav(raw: Vec<u8>, subkey: u16, c: &Config, path: &Path) -> Result<u32> {
    let mut verifier = cridecoder::hca::ClHca::new();
    verifier.decode_header(&raw)?;
    let header = verifier.get_info()?;
    let effective = if subkey == 0 {
        c.cri_key
    } else {
        c.cri_key
            .wrapping_mul(((subkey as u64) << 16) | ((!subkey as u64) + 2))
    };
    verifier.set_key(effective);
    ensure!(header.block_size > 0, "invalid HCA block size");
    let end =
        (header.header_size as u64) + (header.block_count as u64) * (header.block_size as u64);
    ensure!(end <= raw.len() as u64, "truncated HCA");
    for block in raw[header.header_size as usize..end as usize].chunks(header.block_size as usize) {
        let mut bytes = block.to_vec();
        ensure!(
            verifier.test_block(&mut bytes) >= 0,
            "HCA key or block validation failed"
        );
    }
    let mut decoder = cridecoder::HcaDecoder::from_reader(Cursor::new(raw))?;
    decoder.set_encryption_key(c.cri_key, subkey.into());
    let info = decoder.info();
    ensure!(
        (1..=2).contains(&info.channel_count),
        "unsupported channel count"
    );
    let pcm =
        info.block_count as u64 * info.samples_per_block as u64 * info.channel_count as u64 * 2;
    ensure!(pcm <= c.expanded_bytes, "PCM expansion limit");
    let channels = info.channel_count;
    decoder.decode_to_wav(&mut File::create(path)?)?;
    Ok(channels)
}
fn cri(path: &Path, out: &mut Output<'_>) -> Result<()> {
    let c = &out.job.config;
    let mut f = File::open(path)?;
    let mut magic = [0; 4];
    f.read_exact(&mut magic)?;
    let stage = out.job.output.parent().context("work dir")?;
    match &magic {
        b"@UTF" => {
            let table = cridecoder::acb::UtfTable::new(File::open(path)?)?;
            ensure!(table.rows.len() == 1, "invalid ACB header rows");
            let tracks = cridecoder::acb::TrackList::new(&table)?;
            ensure!(
                tracks.tracks.iter().all(|t| !t.is_stream),
                "external AWB dependency is not supported yet"
            );
            let awb = table.rows[0]
                .get("AwbFile")
                .and_then(|v| v.as_bytes())
                .context("missing embedded AWB")?;
            check_awb_references(awb, &tracks.tracks)?;
            // No caller path: do not let a bank probe arbitrary filesystem companion names.
            let waves = cridecoder::extract_acb_unique_to_memory(File::open(path)?, None)?;
            let expected: BTreeSet<_> = tracks
                .tracks
                .iter()
                .map(|t| (t.is_stream, t.stream_awb_id, t.wav_id))
                .collect();
            ensure!(waves.len() == expected.len(), "missing ACB waveforms");
            ensure!(!waves.is_empty() && waves.len() <= 10_000, "waveform count");
            let bytes: usize = waves.iter().map(|v| v.data.len()).sum();
            ensure!(bytes as u64 <= c.expanded_bytes, "waveform budget");
            for (i, w) in waves.into_iter().enumerate() {
                ensure!(
                    w.extension.eq_ignore_ascii_case("hca"),
                    "unsupported waveform codec"
                );
                let mut identities: Vec<_> = w.cues.iter().map(|c| c.cue_id).collect();
                identities.sort();
                let stable_id =
                    format!("wave-{}", crypto::digest(&serde_json::to_vec(&identities)?));
                let tmp = stage.join(format!("wave-{i}.wav"));
                let channels = wav(w.data, w.subkey, c, &tmp)?;
                let original_probe = probe(c, &tmp)?;
                let p = out.path("m4a");
                command(
                    c,
                    "encode",
                    &[
                        "-i".into(),
                        tmp.display().to_string(),
                        "-map_metadata".into(),
                        "-1".into(),
                        "-c:a".into(),
                        "aac".into(),
                        "-b:a".into(),
                        if channels == 1 { "96k" } else { "192k" }.into(),
                        "-movflags".into(),
                        "+faststart".into(),
                        "-fs".into(),
                        c.output_bytes.to_string(),
                        p.display().to_string(),
                    ],
                )?;
                let metadata = validate_media(c, &p, false)?;
                compare_tracks(&original_probe, None, &metadata)?;
                let cues: Vec<_> = w
                    .cues
                    .iter()
                    .map(|v| json!({"name":v.name,"id":v.cue_id}))
                    .collect();
                let label = w
                    .cues
                    .first()
                    .map(|v| v.name.clone())
                    .unwrap_or_else(|| format!("wave-{i}"));
                std::fs::remove_file(tmp)?;
                out.add(
                    p,
                    label,
                    "audio/mp4",
                    json!({"probe":metadata,"cues":cues,"stable_id":stable_id}),
                )?;
            }
            Ok(())
        }
        b"CRID" => usm_video(path, out),
        _ => bail!("unsupported CRI container"),
    }
}
fn decode_text(raw: Vec<u8>, limit: u64) -> Result<Vec<u8>> {
    if !raw.starts_with(&[0x1f, 0x8b]) {
        ensure!(raw.len() as u64 <= limit, "text expansion limit");
        return Ok(raw);
    }
    let mut data = vec![];
    flate2::read::MultiGzDecoder::new(raw.as_slice())
        .take(limit + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "text expansion limit");
    Ok(data)
}

fn compare_tracks(input: &Value, audio: Option<&Value>, output: &Value) -> Result<()> {
    let mut expected = Vec::new();
    for probe in std::iter::once(input).chain(audio) {
        let tracks = probe["streams"]
            .as_array()
            .context("missing source tracks")?;
        ensure!(tracks.len() == 1, "ambiguous source tracks");
        expected.push((probe, &tracks[0]));
    }
    let actual = output["streams"]
        .as_array()
        .context("missing output tracks")?;
    ensure!(
        actual.len() == expected.len(),
        "output track count mismatch"
    );
    for (probe, src) in expected {
        let kind = src["codec_type"].as_str().context("source track type")?;
        let dst = actual
            .iter()
            .find(|s| s["codec_type"] == kind)
            .context("output track missing")?;
        let duration = |p: &Value, s: &Value| {
            s["duration"]
                .as_str()
                .or_else(|| p["format"]["duration"].as_str())
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|n| n.is_finite() && *n >= 0.0)
        };
        ensure!(
            (duration(probe, src).context("source duration unavailable")?
                - duration(output, dst).context("output duration unavailable")?)
            .abs()
                <= 0.1,
            "media duration mismatch or truncated output"
        );
        match kind {
            "audio" => ensure!(
                src["channels"] == dst["channels"] && src["sample_rate"] == dst["sample_rate"],
                "audio format changed"
            ),
            "video" => {
                for dim in ["width", "height"] {
                    let n = src[dim].as_u64().context("video dimension")?;
                    ensure!(
                        dst[dim].as_u64() == Some(n.div_ceil(2) * 2),
                        "video dimension changed"
                    );
                }
            }
            _ => bail!("unsupported source track"),
        }
    }
    Ok(())
}

fn check_awb_references(data: &[u8], tracks: &[cridecoder::acb::Track]) -> Result<()> {
    ensure!(
        data.len() >= 16 && data.starts_with(b"AFS2"),
        "invalid embedded AWB"
    );
    let count = u32::from_le_bytes(data[8..12].try_into()?) as usize;
    let offset_size = data[5] as usize;
    let id_size = data[6] as usize;
    let alignment = u16::from_le_bytes(data[12..14].try_into()?) as usize;
    ensure!(
        count <= 10_000
            && matches!(offset_size, 2 | 4)
            && matches!(id_size, 2 | 4)
            && alignment > 0,
        "unsupported AWB header"
    );
    let table_end = 16 + count * id_size + (count + 1) * offset_size;
    ensure!(table_end <= data.len(), "truncated AWB table");
    let number = |b: &[u8]| {
        b.iter()
            .enumerate()
            .map(|(i, b)| (*b as u32) << (i * 8))
            .sum::<u32>()
    };
    let mut ids = BTreeSet::new();
    for i in 0..count {
        ensure!(
            ids.insert(number(&data[16 + i * id_size..16 + (i + 1) * id_size])),
            "duplicate AWB wave ID"
        );
    }
    let offsets = &data[16 + count * id_size..table_end];
    let mut previous = table_end;
    for i in 0..=count {
        let n = number(&offsets[i * offset_size..(i + 1) * offset_size]) as usize;
        ensure!(n >= previous && n <= data.len(), "invalid AWB offset");
        let start = if i < count {
            n.div_ceil(alignment) * alignment
        } else {
            n
        };
        ensure!(start <= data.len(), "invalid AWB alignment");
        previous = start;
    }
    for t in tracks {
        ensure!(
            t.wav_id >= 0 && ids.contains(&(t.wav_id as u32)),
            "ACB references a missing AWB waveform"
        );
    }
    Ok(())
}

pub fn worker_entry(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path)?;
    ensure!(bytes.len() <= 16 << 20, "worker request limit");
    let job: Job = serde_json::from_slice(&bytes)?;
    let result = match run(&job) {
        Ok(files) => WorkerResult {
            empty: files.is_empty(),
            files,
            error: None,
        },
        Err(e) => WorkerResult {
            files: vec![],
            error: Some(format!("{e:#}")),
            empty: false,
        },
    };
    File::create(path.with_extension("result.json"))?.write_all(&serde_json::to_vec(&result)?)?;
    Ok(())
}

fn verify_decode(c: &Config, path: &Path) -> Result<()> {
    command(
        c,
        "verify",
        &[
            "-i".into(),
            path.display().to_string(),
            "-map".into(),
            "0".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ],
    )
}
fn video_track(probe: &Value) -> Result<&Value> {
    probe["streams"]
        .as_array()
        .context("missing video streams")?
        .iter()
        .find(|v| v["codec_type"] == "video")
        .context("missing video track")
}
fn check_frames(probe: &Value, info: &usm::VideoInfo) -> Result<()> {
    let track = video_track(probe)?;
    let count = track["nb_read_frames"]
        .as_str()
        .and_then(|v| v.parse::<u32>().ok())
        .context("decoded frame count unavailable")?;
    ensure!(
        count == info.frames,
        "USM frame count mismatch: decoded {count}, declared {}",
        info.frames
    );
    Ok(())
}
fn validate_ivf(path: &Path, info: &usm::VideoInfo) -> Result<()> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let mut h = [0; 32];
    f.read_exact(&mut h)?;
    ensure!(
        &h[..4] == b"DKIF" && h[4..8] == [0, 0, 32, 0] && &h[8..12] == b"VP90",
        "invalid IVF header"
    );
    let word = |p| u32::from_le_bytes(h[p..p + 4].try_into().unwrap());
    ensure!(
        word(16) as u64 * info.fps_d as u64 == word(20) as u64 * info.fps_n as u64
            && word(24) == info.frames,
        "IVF/USM timebase or frame count mismatch"
    );
    ensure!(
        u16::from_le_bytes(h[12..14].try_into()?) as u32 == info.width
            && u16::from_le_bytes(h[14..16].try_into()?) as u32 == info.height,
        "IVF/USM dimensions mismatch"
    );
    let mut offset = 32;
    let mut count = 0;
    use std::io::{Seek, SeekFrom};
    while offset < size {
        let mut frame = [0; 12];
        f.read_exact(&mut frame)?;
        let len = u32::from_le_bytes(frame[..4].try_into()?) as u64;
        let pts = u64::from_le_bytes(frame[4..].try_into()?);
        ensure!(
            size - offset >= 12 && pts == count && len > 0 && len <= size - offset - 12,
            "invalid IVF packet/PTS"
        );
        offset += 12 + len;
        f.seek(SeekFrom::Start(offset))?;
        count += 1;
    }
    ensure!(
        count == info.frames as u64,
        "IVF actual packet count mismatch"
    );
    Ok(())
}
fn normalize_adx(c: &Config, path: &Path, stage: &Path) -> Result<PathBuf> {
    let raw = std::fs::read(path)?;
    let (normalized, info) = adx::normalize(&raw, c.expanded_bytes)?;
    let input = stage.join("normalized.adx");
    std::fs::write(&input, normalized)?;
    let output = stage.join("normalized.wav");
    command(
        c,
        "decode",
        &[
            "-i".into(),
            input.display().to_string(),
            "-af".into(),
            format!("atrim=end_sample={}", info.samples),
            "-c:a".into(),
            "pcm_s16le".into(),
            output.display().to_string(),
        ],
    )?;
    let p = probe(c, &output)?;
    let audio = &p["streams"][0];
    ensure!(
        audio["channels"].as_u64() == Some(info.channels as u64)
            && audio["sample_rate"].as_str() == Some(&info.rate.to_string())
            && audio["duration_ts"].as_u64() == Some(info.samples as u64)
            && audio["time_base"].as_str() == Some(&format!("1/{}", info.rate)),
        "ADX decoded sample count/format mismatch"
    );
    std::fs::remove_file(input)?;
    Ok(output)
}
fn usm_video(path: &Path, out: &mut Output<'_>) -> Result<()> {
    let c = &out.job.config;
    let stage = out.job.output.parent().context("work dir")?;
    let mode = c.decryption(&out.job.target.key);
    let key = if mode == usm::Decryption::Key {
        Some(c.cri_key)
    } else {
        None
    };
    let streams = usm::extract(path, stage, key, c.expanded_bytes)?;
    let color = streams
        .iter()
        .find(|s| &s.signature == b"@SFV")
        .context("no USM video")?;
    let info = color.video.as_ref().context("no USM timing")?;
    let alpha = streams.iter().find(|s| &s.signature == b"@ALP");
    if let Some(alpha) = alpha {
        let a = alpha.video.as_ref().context("alpha header")?;
        ensure!(
            a.frames == info.frames
                && a.fps_n as u64 * info.fps_d as u64 == info.fps_n as u64 * a.fps_d as u64
                && a.display_width == info.display_width
                && a.display_height == info.display_height,
            "alpha timing/dimensions mismatch"
        );
    }
    for stream in streams.iter().filter(|s| s.video.is_some()) {
        let metadata = stream.video.as_ref().unwrap();
        if metadata.codec == 9 {
            validate_ivf(&stream.path, metadata)?;
        } else {
            let mut file = File::open(&stream.path)?;
            let mut head = [0; 68];
            let n = file.read(&mut head)?;
            ensure!(
                head[..n].starts_with(&[0, 0, 1, 0xb3])
                    || (n == 68
                        && head[..64].iter().all(|v| *v == 0)
                        && head[64..] == [0, 0, 1, 0xb3]),
                "USM MPEG header mismatch"
            );
        }
        let p = probe(c, &stream.path)?;
        check_frames(&p, metadata)?;
        let v = video_track(&p)?;
        ensure!(
            v["width"].as_u64() == Some(metadata.width as u64)
                && v["height"].as_u64() == Some(metadata.height as u64),
            "USM decoded dimensions mismatch"
        );
        verify_decode(c, &stream.path)?;
    }
    let mut audio = None;
    let mut audio_probe = None;
    if let Some(stream) = streams.iter().find(|s| &s.signature == b"@SFA") {
        let p = match stream.audio_codec {
            Some(2) => normalize_adx(c, &stream.path, stage)?,
            Some(4) => {
                let p = stage.join("audio.wav");
                wav(std::fs::read(&stream.path)?, 0, c, &p)?;
                p
            }
            _ => bail!("unsupported USM audio codec"),
        };
        audio_probe = Some(probe(c, &p)?);
        audio = Some(p);
    }
    let p = out.path("mp4");
    let mut args = vec![
        "-r".into(),
        info.fps(),
        "-i".into(),
        color.path.display().to_string(),
    ];
    if let Some(audio) = &audio {
        args.extend(["-i".into(), audio.display().to_string()]);
    }
    args.extend(["-map".into(), "0:v:0".into()]);
    if audio.is_some() {
        args.extend([
            "-map".into(),
            "1:a:0".into(),
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            if audio_probe.as_ref().unwrap()["streams"][0]["channels"] == 1 {
                "96k"
            } else {
                "192k"
            }
            .into(),
        ]);
    }
    args.extend([
        "-map_metadata".into(),
        "-1".into(),
        "-c:v".into(),
        "libx264".into(),
        "-threads".into(),
        c.ffmpeg_threads.to_string(),
        "-crf".into(),
        "20".into(),
        "-preset".into(),
        "medium".into(),
        "-vf".into(),
        format!(
            "crop={}:{}:0:0:exact=1,pad=ceil(iw/2)*2:ceil(ih/2)*2",
            info.display_width, info.display_height
        ),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-vsync".into(),
        "0".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-fs".into(),
        c.output_bytes.to_string(),
        p.display().to_string(),
    ]);
    command(c, "encode", &args)?;
    let metadata = validate_media(c, &p, true)?;
    check_frames(&metadata, info)?;
    let expected = json!({"streams":[{"codec_type":"video","width":info.display_width,"height":info.display_height,"duration":info.duration().to_string()}]});
    compare_tracks(&expected, audio_probe.as_ref(), &metadata)?;
    out.add(p,out.job.target.key.clone(),"video/mp4",json!({"probe":metadata,"usm":info,"decryption":mode,"role":if alpha.is_some(){"color"}else{"video"},"stable_id":"video-color"}))?;
    if let Some(alpha) = alpha {
        let p = out.path("mkv");
        command(
            c,
            "encode",
            &[
                "-r".into(),
                info.fps(),
                "-i".into(),
                alpha.path.display().to_string(),
                "-map".into(),
                "0:v:0".into(),
                "-an".into(),
                "-vf".into(),
                format!(
                    "crop={}:{}:0:0:exact=1",
                    info.display_width, info.display_height
                ),
                "-c:v".into(),
                "ffv1".into(),
                "-threads".into(),
                c.ffmpeg_threads.to_string(),
                "-pix_fmt".into(),
                "gray".into(),
                "-vsync".into(),
                "0".into(),
                "-fs".into(),
                c.output_bytes.saturating_sub(out.total).to_string(),
                p.display().to_string(),
            ],
        )?;
        let metadata = probe(c, &p)?;
        check_frames(&metadata, info)?;
        let v = video_track(&metadata)?;
        ensure!(
            metadata["streams"].as_array().is_some_and(|s| s.len() == 1)
                && v["codec_name"] == "ffv1"
                && v["pix_fmt"] == "gray"
                && v["width"] == info.display_width
                && v["height"] == info.display_height,
            "alpha output mismatch"
        );
        let duration = metadata["format"]["duration"]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .context("alpha duration missing")?;
        ensure!(
            (duration - info.duration()).abs() <= 0.01,
            "alpha duration mismatch"
        );
        verify_decode(c, &p)?;
        out.add(p,format!("{}.alpha",out.job.target.key),"video/x-matroska",json!({"probe":metadata,"usm":info,"decryption":mode,"role":"alpha-mask","stable_id":"video-alpha","composition":"straight alpha: gray / 255; crop color to display size; pair frames by index"}))?;
    }
    Ok(())
}

#[cfg(test)]
mod review_tests {
    use super::*;
    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
    #[test]
    fn gzip_budget_and_trailing_members() {
        assert!(decode_text(gz(&[1; 100]), 10).is_err());
        let mut data = gz(b"first");
        data.extend(gz(b"second"));
        assert_eq!(decode_text(data, 11).unwrap(), b"firstsecond");
        let mut invalid = gz(b"first");
        invalid.extend(b"junk");
        assert!(decode_text(invalid, 100).is_err());
    }
    fn audio(seconds: &str) -> Value {
        json!({"streams":[{"codec_type":"audio","channels":1,"sample_rate":"48000","duration":seconds}],"format":{"duration":seconds}})
    }
    #[test]
    fn validate_tracks_not_container_duration() {
        let video = json!({"streams":[{"codec_type":"video","width":101,"height":99,"duration":"2"}],"format":{"duration":"2"}});
        let mut result = json!({"streams":[{"codec_type":"video","width":102,"height":100,"duration":"2"},{"codec_type":"audio","channels":1,"sample_rate":"48000","duration":"3"}],"format":{"duration":"3"}});
        compare_tracks(&video, Some(&audio("3")), &result).unwrap();
        result["streams"][1]["duration"] = json!("1");
        assert!(compare_tracks(&video, Some(&audio("3")), &result).is_err());
        result["streams"][1]["duration"] = json!("3");
        result["streams"][1]["channels"] = json!(2);
        assert!(compare_tracks(&video, Some(&audio("3")), &result).is_err());
    }
    #[test]
    fn missing_wave_reference_must_not_fall_back_to_zero() {
        let mut bytes = b"AFS2".to_vec();
        bytes.extend([2, 4, 2, 0]);
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(32u16.to_le_bytes());
        bytes.extend(0u16.to_le_bytes());
        bytes.extend(0u16.to_le_bytes());
        bytes.extend(32u32.to_le_bytes());
        bytes.extend(64u32.to_le_bytes());
        bytes.resize(64, 0);
        let mut track = cridecoder::acb::Track {
            cue_id: 0,
            name: "fixture".into(),
            wav_id: 0,
            enc_type: 2,
            is_stream: false,
            stream_awb_id: 0,
        };
        check_awb_references(&bytes, &[track.clone()]).unwrap();
        track.wav_id = 1;
        assert!(check_awb_references(&bytes, &[track]).is_err());
        bytes[12..14].copy_from_slice(&0u16.to_le_bytes());
        assert!(check_awb_references(&bytes, &[]).is_err());
    }
}
