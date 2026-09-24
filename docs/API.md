# HTTP API v1

Base URL: `http://127.0.0.1:8091`. No authentication. Requests use JSON; application
errors contain an `error` string. This alpha API is experimental. Paths identify
opaque service IDs, never arbitrary local paths or URLs.

| Method and path | Result |
|---|---|
| GET /healthz | Liveness |
| GET /readyz | SQLite/storage readiness and reserved temporary bytes |
| POST /v1/catalogs/refresh | 202, catalog-refresh task |
| GET /v1/catalogs | Retained snapshots and current flag |
| GET /v1/assets | Logical keys and types |
| POST /v1/preflight | Dependency/capability report without downloads |
| POST /v1/exports | 202, export task; completed work is reused |
| GET /v1/tasks/{id} | Task state and per-resource results |
| POST /v1/tasks/{id}/cancel | Request cancellation, safe for completed tasks |
| GET /v1/exports/{id} | Published manifest |
| GET or HEAD /v1/files/{id} | Immutable file, Range and ETag supported |

`GET /v1/assets` accepts `snapshot`, `prefix`, `resource_type`, `offset` (default
0) and `limit` (default 100, capped at 1000). Supply a snapshot while paginating
to avoid switching versions. Aliases sharing the same source are resolved to one
logical asset; genuinely ambiguous keys are not offered as exportable entries.

`POST /v1/exports` accepts exactly one of a nonempty `keys` array or a `prefix`
string, plus an optional `snapshot`, `selector` and `archive` (default false). The empty prefix explicitly selects every
catalog key, including unsupported resource classes; it is not the recommended
way to export only supported assets. Use known prefixes. Keys are sorted and
deduplicated. Nonexistent/unsupported keys produce resource-level failures.

```json
{"snapshot":"OPTIONAL_SNAPSHOT_ID","keys":["Live/MusicScore/0007/0007_03"]}
```

Tasks expose `id`, `kind`, `state`, `snapshot`, `total`, `completed`, `results`,
`error`, `created`, and `updated` (Unix seconds). New export tasks also retain the
complete selection in `keys` (absent on older records). Progress is checkpointed every
20 completed resources and at task termination, rather than for every byte.
States are `queued`, `running`,
`succeeded`, `partial`, `failed`, or `cancelled`. Result entries have `key`,
`export_id` and `error`. On cancellation, every selected key receives a result,
including unstarted keys with `error: "cancelled"`. `completed` counts accounted
results, including these cancellations. Published exports remain available.
On restart, queued/running tasks become failed; missing results from their stored
`keys` receive an interruption error. Legacy tasks without `keys` retain their
existing results because the original selection cannot be reconstructed reliably.
There is no automatic resource retry loop.

Checkpoint write errors set `task.error`, stop scheduling work and produce a
`failed` terminal state. A failed final write receives one retry to persist the
failure itself. If both writes fail, GET for that task returns the terminal
in-memory result; readiness and new submissions return 503 until storage is repaired
and the service restarts. This fallback is volatile: a process crash loses its
latest progress, while startup accounts for remaining persisted selections as
interrupted. Successfully published exports remain reusable.

Subprocess errors keep the `error` string interface, prefixed with a stable code:
`media_probe_failed`, `media_decode_failed`, `media_encode_failed`, `media_verify_failed`,
`process_spawn_failed`, `process_output_limit`, `process_signal`,
`process_cpu_limit`, `worker_spawn_failed`, `worker_exit_failed`, `worker_signal`,
`worker_cpu_limit`, `worker_wait_failed`, `worker_timeout`, or `worker_cancelled`.
The summary includes tool, stage, exit code/signal, elapsed milliseconds and an
opaque `diagnostic_id`. SIGKILL is reported as a signal, not assumed to mean OOM
or CPU exhaustion. Startup tool failures use `process_startup_failed` or
`process_startup_timeout_or_wait_failed`. Raw stderr is never inserted into this
summary. Private details live in `data_dir/diagnostics/ID.json`, with no HTTP route.
If writing diagnostics fails the summary uses `diagnostic_id=unavailable`.
The internal media exec wrapper may report a missing target executable as a
stage failure with its OS error captured in that private diagnostic.

Manifests contain `id`, `snapshot`, `key`, `profile`, `sources` and `files`. Sources
record downloaded InternalIds, provider/options, and original download SHA256.
Each file has
`id`, `name` (service-generated basename), `label` (original name), `media_type`,
`bytes`, `sha256` and format-specific `metadata`. Labels are not unique; use IDs.
New manifests also include `selected_location`, `empty` and transformation
`options`; older manifests default these fields when read. An empty atlas can
succeed with zero files only when its packed sprites, packed names and render
data are all empty. Nonempty unresolved atlas data still fails.
Metadata may include dimensions, cue references, and ffprobe output.

Equivalent export requests receive different task IDs but share active work and
the same published export identity. GET never initiates downloads. Source identity
includes region/CDN/version/locale/catalog SHA256; export identity also includes
the format profile. Catalog remote hash is an update hint, not an independently
verified cryptographic checksum.

Missing records return 404, exhausted task queue returns 429, and invalid requests
return 400. HTTP parsing/body-limit failures use Axum's corresponding 4xx status.
Operational export failures appear in task results, not as an empty success.
File responses may return 206, 304 or 416. Mismatched If-Range falls back to a full
response. Missing physical files return 404 even with a matching conditional
header; error responses are not immutable cache entries. There is no
DELETE/automatic eviction endpoint in this alpha.

## Selection and preflight

`selector` accepts optional `expected_type` and `location_id`; both constraints
must match. A location ID requires exactly one selected key. Omitted selectors
preserve rejection of genuinely ambiguous locations, while equivalent source
aliases still resolve. Selectors are part of the cache identity. For example:

```json
{"keys":["Example/Sprite"],"selector":{"expected_type":"UnityEngine.Sprite"}}
```

`POST /v1/preflight` accepts the same shape and returns the fixed snapshot,
profile and one report per key: `status` is `remote`, `local`, `missing`,
`ambiguous`, or `unsupported`. Candidate location IDs/types and dependency
IDs/InternalIds allow the caller to locate the missing link. For CRI wrappers
with only Unity dependencies, `payload: "embedded_cri_candidate"` means export
must still validate the exact serialized implementation and contained bank.
Preflight checks availability/size, not full payload hashes or codec validity.

`archive: true` exports validated Unity bundles and their dependencies; it does
not turn unsupported meshes/scenes/animations into usable converted objects.
Archive and preview results have distinct cache identities.

USM plaintext overrides are operator configuration, not request-supplied keys.
See [media profile](MEDIA.md) and [local sources/tree/S3](EXPORT.md). New exports
use v2; existing v1 manifests/files remain accessible by their old IDs. Re-submit
requests to populate v2. Selection, decryption, threads, tool versions and pinned
local sources participate in v2 cache identity.
