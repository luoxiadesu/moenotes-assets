# Media profile v3

`json-png-aac-h264-mask-http-v3` uses HTTP-only dependency planning. Media
conversion parameters remain those of v2. New export IDs
include the profile, catalog snapshot, exact key/selector, archive mode, chosen
USM decryption mode, a digest of the configured CRI key, FFmpeg thread count,
tool-version identity and the HTTP dependency policy. Old IDs and manifests
remain accessible. Re-submit requests to build v3; there is no in-place migration.
Changing an executable without changing its reported version is outside this
identity guarantee. Service configuration and binaries must remain fixed while
running.

## USM

The in-project, bounded demuxer indexes streams by signature and channel and
accepts video-first/audio-first headers. It separates metadata and media chunks,
requires per-stream contents-end markers and rejects unknown/extra streams,
truncated chunks and data after termination. The packet mask routines are adapted
from MIT-licensed cridecoder 0.3.5; see the third-party notice. ACB/HCA handling
continues to use that locked dependency.

VIDEO_HDRINFO supplies rational frame rate, total frames, encoded and display
sizes. MPEG's estimated bitrate duration is not used as an integrity oracle.
Every source and output video must decode the declared number of frames; input
frames receive the USM timebase and no frames are duplicated to fill gaps. IVF
additionally checks VP9 headers, rational timebase, packet count and monotonic
unit-step PTS against USM metadata. Display padding is cropped from the origin;
color output is padded to even dimensions for H.264. Source and output are fully
decoded with `-xerror`; audio tracks are validated independently.

`usm_decryption = "key"` is the default. Use `"plaintext"` explicitly, globally
or per logical key in `usm_decryption_overrides`, for known plaintext resources.
There is no magic-byte or decode-error fallback. Manifests record the selected
mode, and changing it creates a new cache identity. Plaintext USM mode only
controls the USM packet mask; HCA encryption has its own validated key path.

USM video packets shorter than 0x240 bytes are not masked. This boundary matters:
masking 512–575-byte packets corrupts otherwise valid short tail frames. Optional
64-byte zero MPEG preambles are accepted; the complete stream must still pass
frame-count and strict decoding checks.

## ADX

Supported input is encoding 3, 18-byte ADPCM blocks, 4-bit mono/stereo, header
version 3/4, unencrypted ADX inside the already demasked container. The adapter
checks `(c)CRI`, channels, rate, sample count, expected data size and a zero-padded
`0x8001` footer with an exact length. It strips only that footer and pads complete
ADPCM blocks to FFmpeg's 128-block packet boundary. Strict PCM decode is trimmed
to the original sample count and checked exactly before AAC encoding. Corrupt,
missing, unexpected or truncated tails fail. No synthetic silence is appended
to published audio and strict errors are not disabled.

## Alpha

A USM with one alpha stream publishes both color MP4 and FFV1 gray Matroska.
The mask must match color frame count, rational rate and display dimensions.
Artifact metadata identifies `color` and `alpha-mask`; `stable_id` pairs them.
Use each mask's gray value / 255 as straight alpha, pair frames by index, and crop
the color's even padding to the declared display size. The MP4 alone is not a
complete transparent asset. The mask is lossless relative to FFmpeg's decoded
gray conversion; synthetic per-frame pixel comparisons cover this contract.
Proprietary runtime composition effects are outside this export profile.

## Embedded ACB and Unity limits

When a CRI wrapper has no external raw payload, the worker resolves its exact
AssetBundle container object and checks the exact wrapper/reference type trees and reads the selected
`implementation.rid` from the matching little-endian serialized layout. The
byte array is read directly under a budget rather than materialized as millions
of generic type-tree values. Only `CriWare.Assets.CriSerializedBytesAssetImpl` in
`CriMw.CriWare.Assets.Runtime` is accepted. Its byte array is validated as a CRI
container and follows the normal waveform path. Missing references, another
implementation type, external/additional AWB pointers or ambiguous raw payloads
fail explicitly. Playback-only MonoScript bundles are not needed for this
self-contained type-tree representation.

An atlas is `empty` only when packed sprites, packed names and render data are
all empty. Its manifest has `empty: true` and zero files; unresolved render data
is an error. Unsupported Unity types are not converted to models or animation.
Use explicit `archive: true` to publish CRC-validated Unity bundles and their
available HTTP bundle dependencies as `application/vnd.unity`; this is a container archive,
not a preview or a decoded model.

Font data/subresources do not enter preview conversion; see EXPORT.md. Package
references remain in their retention JSON even when no font bytes are remotely
available. Explicit archive also supports unchanged CRI container bytes.

## Split song containers

`Fwk.Sound.SplitAcbData` is a supported preview input. The exporter resolves the
selected wrapper's `_chunks` TextAsset references in serialized array order,
concatenates their bytes and XORs each byte with `0x5a`, matching the native
SplitAcbLoader. This reconstructs one ACB; it is not audio segment concatenation.
The recovered ACB uses the existing strict AWB/HCA/AAC path. Invalid/null chunk
references, non-TextAsset chunks, empty arrays, expansion overflow and invalid
ACB headers fail. Output metadata records the cue-sheet name, chunk count,
reconstructed ACB SHA256 and `ordered-textasset-xor5a-v1` assembly identifier.

The media profile remains v3: this adds a previously unsupported input type and
does not change outputs or cache identities for already supported resources.
Failed tasks have no successful export cache to migrate.
