# Contributing to Potluck

This is the developer guide. The page for people who run Potluck is [README.md](README.md).

## Requirements

- Rust 1.99 or newer (the `rust-version` in [`Cargo.toml`](Cargo.toml)).
- git, on your `PATH`. The server runs it to serve every workspace, and the tests run it too.
- jq, for [`scripts/third-party-crates.sh`](scripts/third-party-crates.sh) and [`scripts/check-public-api.sh`](scripts/check-public-api.sh).
- podman or docker, only to build the image.

## Build and run

```sh
cargo build --release -p potluck
```

The binary lands in `target/release/potluck`. To run it from a checkout, with its data in `./data`:

```sh
cargo run -p potluck -- serve --data ./data --public-url http://127.0.0.1:8080
```

It listens on `127.0.0.1:8080`. In a second shell, `cargo run -p potluck -- admin --data ./data user create alice --server-admin` creates the first account, the way [First admin](README.md#first-admin) does in the container. `cargo run -p potluck -- --help` lists the subcommands and their flags.

## Build the image

The image is built from [`Containerfile`](Containerfile):

```sh
podman build -t ghcr.io/phongguy/potluck:latest .
```

With docker, `docker build` takes the same arguments plus `-f Containerfile`. [`scripts/container-smoke.sh`](scripts/container-smoke.sh) builds the image and runs a smoke test against it on 127.0.0.1: `ENGINE=podman scripts/container-smoke.sh`.

## Layout

- [`crates/potluck`](crates/potluck): the server, its admin CLI and the pre-receive hook, all in one `potluck` binary.
- [`crates/potluck-kind`](crates/potluck-kind): the rule-set contract between Potluck and the apps. Each app's rules crate implements it.
- `apinata-rules` and `dbento-rules`: each app's rules for its kind. They live in the app's own repository, and the workspace `Cargo.toml` pins each one by a git tag.
- [`deploy`](deploy) and [`compose.yaml`](compose.yaml): the Quadlet unit and the Compose file the README uses.
- [`scripts`](scripts): the CI helpers, the release script and the notices generator.

The workspace members are `crates/potluck` and `crates/potluck-kind`.

## Checks

A change is ready when all of these pass:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -Dwarnings
cargo test --workspace
```

CI also runs the `rules` job, which is [`scripts/rules-crates.sh`](scripts/rules-crates.sh):

```sh
scripts/rules-crates.sh test Cargo.lock
```

It checks that `Cargo.lock` holds exactly one `potluck-kind`, the workspace's own. Then, for each rules crate pinned in the lock, it clones the app's repository at the pinned tag, checks that the tag still points at the pinned commit, and runs that crate's tests there. It needs network access to GitHub.

### Tests

Each crate has one integration test binary, named `integration`. Its `tests/main.rs` declares every other file in `tests/` as a module, and the crate's `Cargo.toml` sets `autotests = false`. A new test file must be added to `tests/main.rs`, or it never runs.

Tests reach only 127.0.0.1. Every test server gets an ephemeral port and a temporary data directory, and no test touches your real config or data directories.

### Public API

No type from a dependency appears in a public signature, and public types are `#[non_exhaustive]`. CI checks the first rule for axum, tower, http and hyper types. To run that check yourself:

```sh
RUSTC_BOOTSTRAP=1 cargo rustdoc -p potluck --lib --locked -- -Z unstable-options --output-format json
scripts/check-public-api.sh target/doc/potluck.json
```

## What CI runs

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on every pull request and every push to `main`:

- `gate`: `cargo fmt --check`, clippy with `-Dwarnings`, `cargo test --workspace --locked` and the public API check.
- `rules`: `scripts/rules-crates.sh test Cargo.lock`, as above.
- `zizmor`: audits the workflows.
- `container`: builds the image and runs the smoke test under podman. It informs and does not block.

[`.github/workflows/attribution.yml`](.github/workflows/attribution.yml) checks every pull request's title, body and commit messages, and fails one that credits an AI agent.

[`.github/workflows/release.yml`](.github/workflows/release.yml) builds and publishes the image from a `v*` tag.

## Rules crates and their tags

Potluck knows no app file format. Each app's push rules and starting files for its kind live in a rules crate in the app's repository, built on `potluck-kind`. The workspace `Cargo.toml` pins each rules crate by a crate-scoped git tag, `<crate>-vX.Y.Z`:

```toml
apinata-rules = { git = "https://github.com/PhongGuy/apinata", tag = "apinata-rules-v0.1.0" }
```

`potluck-kind` is versioned on its own and tagged the same way, as `potluck-kind-vX.Y.Z`. A rules crate depends on `potluck-kind` through this repository's git URL and one of those tags. The `[patch."https://github.com/PhongGuy/potluck"]` section in `Cargo.toml` points that dependency at `crates/potluck-kind`, so the server and every rules crate build against the same copy. Crate tags never take the `v*` form, which is reserved for server releases.

To move to a new rules crate release, change its tag in `Cargo.toml`, run `cargo update -p apinata-rules` (or the crate you changed), then run the checks and `scripts/rules-crates.sh test Cargo.lock`.

## Third-party notices

[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) lists every Rust crate the `potluck` binary links. After a dependency change, regenerate the list:

```sh
scripts/third-party-crates.sh --write
```

The script fails on a crate whose licence is not on its allowlist, and the test suite fails while the list is stale.

## Sending a change

- One issue per branch, and one pull request per issue. The pull request body says `Closes #N`.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/): `<type>(<scope>): <description>`, with the types `feat`, `fix`, `chore`, `refactor`, `docs` and `test`.
- Give the pull request a title in plain words, such as "Split developer docs into CONTRIBUTING.md".
- I merge pull requests with a merge commit, never a squash, so each commit lands as you wrote it.
- Don't credit an AI agent in a commit or a pull request: no AI `Co-authored-by:` trailer and no "Generated with" footer. The attribution check fails a pull request that has one.

## Filing issues

Open an issue with one of the forms: Bug, Feature or Task. Blank issues are off, so pick the form that fits. The Bug form asks what happened, the steps to reproduce it, what you expected and the version you run.
