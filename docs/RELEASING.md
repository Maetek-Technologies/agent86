# Releasing agent86

## Publishing a release

Add notes for it in `docs/releases/<tag>.md`, set the version in the root `Cargo.toml`'s `[workspace.package]` (the only place it is written; both crates inherit it), merge both, and push a `v<version>` tag. `.github/workflows/release.yml` then builds the archives, creates the release with those notes and its SLSA provenance, publishes the npm packages (`npm/build.mjs`), and publishes the library, `agent86-core`, to crates.io (the `crate` job, once the release is out; the binary is not published there).

The crates.io publish uses the repository secret `CARGO_REGISTRY_TOKEN`: a crates.io API token with the `publish-update` scope (and `publish-new` for the very first publish), limited to the crate `agent86-core`. Without it the `crate` job fails on a tag and nothing reaches crates.io; the rest of the release is unaffected, and re-running the job once the secret is set publishes the crate. crates.io never takes a version twice, so a version published by mistake can only be yanked (`cargo yank`), never replaced.

`agent86_core::sdk` is covered by semantic versioning; the library's hidden modules are not. A release that breaks `sdk` needs a new major version.

## Verifying a release

Releases from v1.0.19 on carry SLSA build provenance (`agent86-<tag>.intoto.jsonl`), so you can check that an archive was built by this repository's release workflow with [`slsa-verifier`](https://github.com/slsa-framework/slsa-verifier):

```sh
slsa-verifier verify-artifact <archive> \
  --provenance-path agent86-<tag>.intoto.jsonl \
  --source-uri github.com/Maetek-Technologies/agent86 \
  --source-tag <tag>
```
