# Development and release checks

Keep production source, synthetic tests, English documentation, the dependency
lockfile and license inventory in this repository. Do not commit private catalogs,
game assets, APKs, player credentials, machine-specific configs, benchmark data,
databases or target outputs. Research evidence stays outside the repository.

Before a commit intended for push:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo doc --no-deps --locked
cargo build --release --locked
git diff --check
```

Review the staged diff, update the changelog and affected API/profile contracts,
and check for secrets and unintended files. Changes to parsers or encoders need
negative synthetic tests and applicable private real-resource regressions;
record scope and unresolved limitations without adding copyrighted fixtures.
Changes affecting the container require a local image build/smoke test. Preserve
third-party license notices when adapting source. CI validates but never uploads
game resources or publishes images. Tags and binary/container releases are
separate from pushing source commits.
