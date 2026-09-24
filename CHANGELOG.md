# Changelog

Versions follow MAJOR.MINOR.PATCH, with prerelease identifiers where applicable.

## 0.1.0-alpha.4 - 2026-09-25

- Export SplitAcbData songs/tutorial audio by resolving TextAsset references in
  declared order and recovering the ACB with the native XOR mask. Validate chunk
  references, expansion bounds and the reconstructed header before the existing
  strict audio conversion pipeline.
- Record assembly version, cue-sheet name, chunk count and reconstructed source
  SHA256 in audio metadata. Existing supported-resource cache identities remain
  unchanged.

## 0.1.0-alpha.3 - 2026-09-25

- Use one HTTP-only dependency plan for preflight and exports. Preview workers
  resolve real object/texture references without requiring package-only runtime
  scripts and shaders; omitted entries remain visible in manifests.
- Retain font keys and their aliases without font conversion: reference JSON plus
  available remote bundles, or an explicit package-only reference when no HTTP
  payload exists. Font reference retention does not claim downloaded font bytes.
- Add paginated HTTP resource inventory and a batch archive endpoint covering
  all directly addressable resources, including original CRI containers.
- Remove the alpha.2 local directory provider and `local_source` configuration;
  old configs must remove that field. Archive now means HTTP containers, with
  incomplete runtime dependency closures explicitly identified.
- Use profile v3 and dependency-policy cache identity; keep old exports readable.
  The tree naming contract remains v2 and defaults to the current media profile.

## 0.1.0-alpha.2 - 2026-09-24

- Fix batch checkpoint SQL-pool starvation with independently scheduled, bounded
  resource tasks; retain the original selection and account for cancelled,
  interrupted and panicked resource tasks.
- Surface checkpoint/final persistence errors instead of silently leaving tasks
  running; retain unpersistable terminal results in memory and fail readiness/new
  submissions until storage repair and restart.
- Capture bounded FFmpeg/ffprobe/worker stderr privately, with stage-specific
  error codes, exit status/signals, duration, diagnostic IDs, tool versions and
  media source context. Keep strict media validation.
- API addition: export tasks expose `keys`; cancelled/recovered tasks now include
  results for all known selected keys and count them in `completed`. Old stored
  tasks without a selection remain readable. Rust `Task` gains a `keys` field.

- Add order-independent, bounded USM demuxing with rational container timing,
  strict frame counts, IVF PTS validation, explicit decryption overrides and
  separate lossless alpha masks. Honor the 0x240-byte packet masking boundary.
- Normalize validated ADX footers/packet tails before strict, sample-exact PCM
  decode, fixing both ordinary tails and previously misclassified test media.
- Resolve embedded ACB bytes through the exact serialized implementation; add
  dependency preflight, pinned local sources and explicit type/location selection.
- Represent verified empty atlases as empty manifests; add opt-in validated
  Unity-container archives without claiming model/animation conversion.
- Separate CPU/wall and video limits; include selected processing inputs and
  tool versions in the new v2 profile/cache identity, retaining old IDs.
- Add copy/hardlink export trees, portable stable naming, public upload plans and
  explicit S3 SigV4 conditional creation with full readback, retry/cancellation
  and private recovery journals. Existing research tree paths are not migrated.
- Rust alpha API additions include selector/archive, local/decryption/limit
  configuration, manifest options/empty state and worker archive/empty fields.
  Read the updated API/media/export contracts before upgrading callers.

## 0.1.0-alpha.1 - 2026-09-24

- Add Android Addressables binary catalog snapshots and explicit CDN configuration.
- Add asynchronous HTTP export tasks, dependency downloads, bundle decryption,
  SQLite indexing, cancellation, shared work, and temporary-file cleanup.
- Export TextAsset payloads, PNG textures/sprites/atlases, HCA-based M4A audio,
  and supported USM video as H.264 MP4.
- Add immutable artifact URLs with Range, HEAD and ETag support.
- Add bounded Rust workers and local container packaging.
- Harden deterministic dependency ordering, publication rollback, conditional
  file responses, bounded gzip decoding and CRI media validation before the
  initial public source commit.

The HTTP v1 interface and Rust library are experimental in this alpha. Breaking
changes require explicit release notes; a frozen stable API is not claimed.
