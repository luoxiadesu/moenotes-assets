# Third-Party Components

MoeNotes Assets source code is MIT licensed. This does not relicense dependencies,
FFmpeg, game files, game trademarks, or exported media.

- `unity-rs-core 0.5.1`: MIT; https://github.com/seiunx-dev/unity-rs.
- `cridecoder 0.3.5`: MIT; https://github.com/seiunx-dev/cridecoder.
  `src/usm.rs` adapts its MIT-licensed packet-mask routines; the demuxer and
  bounded metadata reader are implemented in this project. The upstream license
  is retained under `third_party/licenses/cridecoder-0.3.5/LICENSE`.
  Its optional Python feature is disabled. Neither Python nor C# is a runtime dependency.
- Rust dependencies and selected versions are recorded in Cargo.lock. License files
  collected from the corresponding crate sources are under `third_party/licenses`.
  This inventory is not a substitute for a complete legal or security audit.
- FFmpeg and libx264 run as separate executables, not linked into this Rust library.
  The Debian FFmpeg build used by the container enables GPL components, including
  libx264. The complete image must not be described as "MIT-only".
- Debian package copyright and license notices remain under `/usr/share/doc` in the
  runtime image. Redistribution must satisfy the applicable licenses, including
  providing corresponding source for GPL components by a compliant method.
  Merely linking to an upstream repository is not a source-distribution guarantee.

The initial image is for local validation only. No public image publication is
configured. Before distributing an image, record exact installed package versions,
archive corresponding source packages and build instructions, audit the final
image's license set, and choose an appropriate source-distribution mechanism.

No proprietary CRI plugin, official client binary, player credential, catalog, or
game resource is included in this project. Fixed format key material is used solely
for interoperability; it does not grant permission to redistribute game content.
