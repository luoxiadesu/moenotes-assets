# moenotes-assets

A Rust HTTP service for retrieving and exporting Our Notes assets. It reads an
Android Addressables catalog, downloads the selected dependencies, decrypts
supported resources, and publishes usable files on local disk.

Version **0.1.0-alpha.3**. This is an independent interoperability project, not an
official game service. The HTTP v1 and Rust interfaces are experimental.

## Supported Resources

| Input | Output |
|---|---|
| TextAsset, including gzip content | Original JSON, SUS, UTF-8 text, or binary payload |
| Texture2D, Sprite, populated SpriteAtlas | PNG |
| ACB with embedded HCA waveforms | AAC-LC in M4A, with cue-name metadata |
| USM MPEG/VP9, including separate alpha | H.264/AAC MP4 + lossless gray FFV1 mask when present |
| Embedded serialized ACB | AAC-LC M4A, selected by exact wrapper reference |
| Empty SpriteAtlas | Empty manifest, no fabricated image |
| HTTP container archive | CRC-validated Unity bundles or original CRI containers |
| Font assets and their subresources | Remote bundle + catalog reference, without font conversion |

Unity parsing uses `unity-rs-core 0.5.1`; ACB/HCA parsing uses `cridecoder 0.3.5`.
USM uses the bounded in-project demuxer; see [media contracts](docs/MEDIA.md).
FFmpeg runs as a separate process. **No Python, C#, Unity Editor, proprietary CRI
plugin, game login, or player credentials are required at runtime.**

Exports are asynchronous. Single-resource results publish atomically; batches
may partially succeed. Concurrent requests share work, completed exports are
reused, and GET requests never start downloads. Downloads and intermediate files are temporary during preview conversion.
Explicit archive tasks publish retained HTTP containers. Catalog snapshots and old
exported versions remain available after a refresh.

Not supported: arbitrary game versions/platforms, external streaming AWB banks,
CPK, scene/model/animation exports, complex cue playback, song-segment assembly,
multichannel audio and ambiguous multi-track video. Resource retrieval uses HTTP(S) only. Package-only dependencies are listed in
preflight/manifests and never opened or guessed as CDN paths. Missing actual
image/object references still fail during conversion. Ambiguous keys require a
type/location selector. Asset availability and rights remain the publisher's concern.

## Install

Requirements: Linux, Rust 1.98.1, a C compiler/pkg-config for native dependencies,
FFmpeg/ffprobe with AAC, libx264 and FFV1, and `prlimit` from util-linux.

```sh
cargo build --release --locked
cp config.example.toml config.toml
# Set cdn_root to the authorized CDN root for your selected region.
./target/release/moenotes-assets serve config.toml
```

The default listener is `127.0.0.1:8091`. Configuration is loaded once at startup.
Region, locale, and Bili resource version are explicit. This service does not
discover servers or authenticate to the game API.

**There is no HTTP authentication.** All callers can submit costly downloads and
transcodes. Keep the service on a trusted network, or place an authenticated,
rate-limited reverse proxy in front of it. Do not expose it directly to the Internet.

## First Export

Refresh the catalog and poll the returned task until it succeeds:

```sh
curl -X POST http://127.0.0.1:8091/v1/catalogs/refresh
curl http://127.0.0.1:8091/v1/tasks/TASK_ID
curl 'http://127.0.0.1:8091/v1/assets?prefix=Live%2FMusicScore%2F&limit=20'
```

Submit an exact logical key, or use `prefix` instead of `keys` for a batch:

```sh
curl -X POST http://127.0.0.1:8091/v1/exports \
  -H 'Content-Type: application/json' \
  -d '{"keys":["Live/MusicScore/0007/0007_03"]}'
curl http://127.0.0.1:8091/v1/tasks/TASK_ID
curl http://127.0.0.1:8091/v1/exports/EXPORT_ID
curl http://127.0.0.1:8091/v1/files/FILE_ID -o chart.json
```

A task fixes its catalog snapshot at creation. Successful result entries contain
an `export_id`; its manifest lists file IDs, labels, MIME types, sizes, and SHA256
digests. File URLs support Range, HEAD, and ETag. Original asset names are labels,
not trusted filesystem paths.

To retain every directly downloadable resource in the catalog, independent of
preview support:

```sh
curl 'http://127.0.0.1:8091/v1/resources?limit=100'
curl -X POST http://127.0.0.1:8091/v1/resources/archive \
  -H 'Content-Type: application/json' -d '{}'
```

This returns a normal task for all HTTP resource locations. Unity containers are
decrypted and CRC-checked; CRI containers are preserved without transcoding.
Fonts stay outside preview conversion: remote font bundles are archived, while
package-only font keys produce a reference JSON clearly stating that the font
bytes were not downloaded. Existing locally saved font data can remain offline.

## Storage and Limits

The configured `data_dir` contains SQLite, retained catalog snapshots, immutable
export directories, and temporary job directories. Only one service may own it.
Keep the volume private to the service user. Do not edit files under a running
service or delete directories directly while the index still references them.

Defaults target a multicore machine with at least 8 GiB RAM: 8 downloads, 2 Rust
workers, 1 concurrent video, and 4 FFmpeg threads. Queue capacity is 128 active
tasks, with at most 50,000 selected keys per task. Input, expansion, output,
temporary-disk, worker address-space, CPU and wall-time limits are configurable.
Temporary space uses conservative reservations; a busy service may reject work
before the filesystem is full. Configure a filesystem quota/container memory
limit as an additional system boundary. Disk-full errors are reported, not retried
indefinitely. Final exports have no automatic eviction.

Downloads are streamed and decrypted into temporary files. Small files may be
loaded into memory by the Unity worker; large files retain file-backed sources.
This is not a promise of zero-copy parsing or unpacking before a download ends.
Cancellation terminates worker process groups. Shared work continues while
another caller still needs it. Restart clears temporary files and marks unfinished
tasks failed and accounts for every key in new tasks; resubmit failed keys to retry.
Task persistence failures are surfaced as failed tasks, with readiness/new work
blocked if final progress can only be retained in memory. Subprocess failures have
stage/exit/signal summaries and private, bounded stderr diagnostics under
`data_dir/diagnostics`; apply an operator retention policy to that directory. Partial HTTP-byte-range download resume
is not implemented.

Audio uses 96 kbps mono / 192 kbps stereo AAC without normalization. Video uses
H.264 CRF 20, medium, yuv420p and faststart with the USM rational frame rate and
display size, padding odd dimensions to even. ADX normalization is sample-exact;
alpha has an explicit color-plus-mask contract. Video without audio stays silent. Output
media is probed and fully decoded before publication; codec versions may affect
compressed bytes, so SHA256 identifies actual output rather than a universal
cross-version encoding result.

## Preflight and readable files

`POST /v1/preflight` accepts the export selection and reports remote, retained
font, unavailable-HTTP, missing, ambiguous or unsupported resources without
downloading. It uses exactly the same dependency plan as execution. `selector` accepts
`expected_type` or `location_id`; a location ID requires a single key. Use
`archive: true` to retain available HTTP containers. Archives record any omitted
package dependencies and never claim a complete runtime package.

```sh
moenotes-assets export-tree DATA_DIR DESTINATION SNAPSHOT_ID copy
moenotes-assets upload-tree DESTINATION s3.toml
```

See [export contracts](docs/EXPORT.md) for naming, HTTP sources, conditional S3
creation, credentials and immutable migration rules. Upload is an explicit CLI
operation and is never triggered by a service request.

## Container

```sh
docker build -t moenotes-assets:local .
docker volume create moenotes-assets-data
docker run --rm -p 127.0.0.1:8091:8091 \
  -v moenotes-assets-data:/data \
  -v "$PWD/config.toml:/etc/moenotes-assets/config.toml:ro" \
  moenotes-assets:local
```

For the container set `listen = "0.0.0.0:8091"` and `data_dir = "/data"`.
It runs as UID/GID 65532; bind mounts must be writable by that user. The Dockerfile
uses BuildKit Cargo registry and target caches. `/healthz` reports liveness;
`/readyz` checks the index and writable storage. Media encoders are checked at startup.

This image is intended for local validation. Review FFmpeg/libx264 redistribution
requirements before publishing it; see [Third-Party Notices](THIRD_PARTY_NOTICES.md).

## API and Development

See [HTTP API](docs/API.md), [media](docs/MEDIA.md),
[HTTP/tree/S3 export](docs/EXPORT.md), [development rules](docs/DEVELOPMENT.md),
[architecture](docs/ARCHITECTURE.md), and
[changelog](CHANGELOG.md). Internal worker JSON is not a public integration API.

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo doc --no-deps --locked
```

Tests use synthetic files and loopback HTTP, not publisher endpoints. Private
real-resource acceptance data is intentionally excluded from the repository.
Only documented inputs are supported. Container archives require explicit archive
mode and are identified separately from usable previews.

GitHub Actions runs format, lint, tests, documentation and Release builds with
Cargo caching. CI does not publish binaries, images or game resources.
See [Security Policy](SECURITY.md) for deployment boundaries and private reporting.

## License

Project code: [MIT](LICENSE). Dependencies have their own licenses. The container
includes GPL-enabled FFmpeg and is not MIT-only. No license here grants rights
to game resources, trademarks, or redistributed exports.
