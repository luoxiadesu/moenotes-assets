# Architecture

The Linux service owns a single SQLite database and export volume. Catalog
snapshots are immutable and active parsing graphs are shared. HTTP validates
selection requests; each task retains its snapshot. Selection is by Addressables
logical key, with exact AssetBundle container-path resolution rather than a
filename search. Texture2D/Sprite aliases are accepted only when their source
and dependency lists agree.

Active export and download registries hold weak entries. Reference-counted leases
keep payloads alive while consumers need them, then remove temporary directories.
One caller's cancellation does not cancel another caller's lease. Resource gates
serialize cancellation/retry overlap around publication. Queue, download, worker,
video and temporary-storage budgets are distinct. A bounded JoinSet runs resources
independently of task checkpoint writes, so a coordinator waiting for SQL never
suspends the resource futures that need to release pooled SQL connections. The
SQLite pool stays at four connections. Selections are persisted before execution;
failed checkpoints and cancellation account for every selected key.

Downloads disable redirects and environment proxy inheritance, remain under the
configured CDN root, enforce declared size, and decrypt the bundle prefix while
streaming. A worker checks UnityFS block CRC when nonzero. CRI's catalog CRC=0 is
not claimed as a successful checksum comparison. Only HTTP(S) resources are
fetched. One dependency planner drives preflight and
execution, retaining remote inputs and recording omitted package dependencies.
Actual object and texture resolution remains mandatory in preview conversion.
Font keys/aliases are retained as references plus available remote containers.

CRI logical assets with exactly one raw-media dependency can bypass playback-only
ScriptableObject/MonoScript dependencies. The selected raw container is still
validated; ambiguous multi-media wrappers are refused. Self-contained serialized
CRI wrappers select bytes through implementation.rid and an exact implementation
type, without loading playback-only MonoScript dependencies. Embedded waveform
references are decoded, not arbitrary paths inferred from a filename.

Workers run the same executable in a separate process under Linux `prlimit`.
Unity uses bounded file/memory Regions; CRI may materialize waveforms and therefore
also relies on process limits. FFmpeg/ffprobe execute through a small Rust exec
wrapper that sets parent-death signaling. Cancellation kills the worker process
group, and container init handles orphan reaping. stdout/stderr are drained concurrently:
stderr retains a 16 KiB tail and probe stdout a 1 MiB prefix (oversize output
fails). Failures save mode-0600 private diagnostic JSON under a mode-0700 directory;
the API receives only fixed error codes, process metadata and opaque IDs. Tool
versions are recorded at startup; media diagnostics link to export/snapshot/key
and raw input SHA256 through a private worker context. Parent-side launch/signal
failures may lack that media context. Resource completion logs include export ID,
elapsed time and success; existing manifests retain artifact size/SHA validation.
Diagnostics have no automatic retention policy; include this directory in the
service-volume quota and operator log-retention process. These are resource controls,
not a hardened hostile-code sandbox. Add container memory, disk and network policy
for deployment.

Each resource writes to an isolated staging directory. Complete validated outputs
are renamed into the export store on the same filesystem, then committed to SQL.
HTTP exposes only SQL-indexed files. Startup removes unindexed publication
directories and unfinished temporary files, retaining successful exports. A task
may partially succeed; a single resource never advertises partial files.

Preview tasks discard downloaded containers, decoded WAV and demuxed video.
Archive tasks explicitly retain downloaded CRI bytes or decrypted Unity bundles
as indexed artifacts. Preview tasks do not yet reuse these container archives
as a download cache, so changing the format profile downloads inputs again.
Catalog binaries/metadata are retained for reproducibility. Output SHA256 is
recorded after conversion. SHA256 is not used to claim authenticity of publisher
content without an independent trusted expected digest.

Known alpha boundaries: Android binary v2 catalog only; no login or CDN discovery;
no byte-range download resume, automatic retries, multi-instance coordination,
browser UI, authentication, export garbage collector, external AWB discovery,
cue runtime or arbitrary Unity object conversion. Explicit CLI export-tree and
conditional S3 upload are separate from the HTTP service and its local store.
USM timing, alpha and strict ADX contracts are described in MEDIA.md; naming and
immutable migration rules are described in EXPORT.md.
