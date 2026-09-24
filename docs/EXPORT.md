# Local sources, readable trees and S3

## HTTP-only retrieval

The service only fetches catalog InternalIds beginning with HTTP(S), rebased onto
the configured CDN under the existing URL/redirect/TLS constraints. It neither
reads an APK/directory provider nor guesses remote paths for package entries.
`local_source` from alpha.2 is no longer accepted; remove it when upgrading.
Existing offline files and published exports are left untouched.

Preflight and export share one dependency plan (`http-resources-v1`). All remote
Unity dependencies are kept for image/text previews; package-only runtime
scripts/shaders are omitted and listed in the manifest. A successful download
alone is insufficient: unresolved target objects, textures and actual external
references still fail in the worker. Raw CRI wrappers use the unique HTTP payload
or the validated embedded-ACB path.

Font families, including Texture2D/Material aliases of a font key, are retained
without conversion. A remote font produces a reference JSON and validated remote
bundles. A package-only font produces only the reference JSON, with
`package_payload_downloaded: false`; this is not a downloaded font or an empty
atlas. Existing package font bytes stay in the operator's offline archive.

`GET /v1/resources` lists directly addressable HTTP resource locations, including
containers with no preview converter. `POST /v1/resources/archive` with `{}` or
`{"snapshot":"ID"}` submits all such locations as a normal bounded task. Catalog
keys remain mapped to unique resource locations; missing/ambiguous primary-key
mappings are rejected instead of silently skipped. Task limits still apply.

Archive mode now means available HTTP containers, not a complete runtime bundle
closure. Non-HTTP dependencies are listed and `dependency_closure_complete` is
false whenever any were omitted. CRI archives validate length/signature and
preserve original bytes, without claiming codec decode success; Unity archives
retain decrypted bundles with nonzero CRC validation. No local provider is used.

## Readable export

```sh
moenotes-assets export-tree DATA_DIR DESTINATION SNAPSHOT_ID copy [PROFILE]
# Use '-' for all snapshots only when each key has one selected export.
# Optional hardlink mode requires the same filesystem.
```

The optional profile defaults to v3, so retained v1/v2 records do not conflict with
new exports. Pass an old profile explicitly to export an older generation to a
separate tree. The source index is opened read-only. The tree root is the encoded original key,
e.g. `Image/Jacket/.../label~stable-id.png`; hash export directories stay internal.
Color and mask files use `key-basename.mp4` and `key-basename.alpha.mkv`.
Other artifacts use a readable label plus a stable source-identity suffix,
independent of output position. Unity identities use source identity/path ID;
ACB identities use the sorted cue IDs identifying the physical waveform.

Naming version `key-stable-artifact-v2` percent-encodes unsafe/Unicode bytes,
escapes percent, handles Windows reserved names/trailing dots/spaces, and bounds
components. Case/encoding collisions and object keys over 1024 bytes are refused.
Copy is the default, with no symlinks. Hardlinks preserve space but edits also
change the source; treat both trees as immutable. Cross-filesystem hardlinks fail
with a request to use copy.

`_meta/manifest.json` and `_meta/upload-plan.json` contain relative object keys,
asset/label/role, source snapshot/profile, immutable artifact identity, byte count
and SHA256. Empty exports are recorded separately. No machine paths or credentials
are included. Files are verified before and after copying. A tree lock serializes
writers, copying is atomic, and reruns validate and reuse matching objects.
Content conflicts are refused rather than overwritten. Multiple exports for one
key require an explicit snapshot and a fresh tree.

This is a new naming contract. Existing v1 research upload paths are not renamed
or overwritten. To migrate a published tree, create a new tree/bucket or separately
review an explicit old-URL mapping and retain old objects. Do not point this CLI
at a preexisting research tree and assume compatibility.

## Conditional S3 upload

```toml
# s3.toml: no credentials
endpoint = "https://storage.example.invalid"
bucket = "assets-v2"
region = "us-east-1"
concurrency = 4
timeout_seconds = 900
```

Provide `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optional
`AWS_SESSION_TOKEN` through the operator environment, then run:

```sh
moenotes-assets upload-tree DESTINATION s3.toml
```

The path-style S3 client signs SigV4 requests, streams files, uses
`If-None-Match: *`, and writes content type plus SHA256 object metadata. A 412
response is accepted only after a complete GET matches byte count and SHA256.
Every object is read back; ETag is not treated as a content hash. Redirects are
disabled. Each PUT/GET has its own complete request deadline. Bounded concurrency,
three attempts with backoff, Ctrl-C cancellation and a private per-attempt journal
support reruns even if a timed-out PUT already created the object. Mismatched
existing contents never trigger an overwrite. Uploads cannot undo an object
already accepted by S3 before cancellation.

Only after every resource is verified is `_meta/manifest.json` conditionally
published and read back. The local `_meta/upload-local.jsonl` journal is not
uploaded. Resuming re-verifies remote objects rather than trusting a previous
success log. Manifest conflicts also fail; choose an explicit new destination
for a changed publication. Single PUT supports objects up to 5 GiB; multipart,
conditional updates and automatic deletion are not implemented. The endpoint
must correctly honor S3 conditional writes. Upload bandwidth figures include
both PUT and complete GET verification traffic.
