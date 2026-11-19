# Releasing agent86

## Publishing a release

Add notes for it in `docs/releases/<tag>.md`, set the version in `Cargo.toml`, merge both, and push a `v<version>` tag. `.github/workflows/release.yml` then builds the archives, creates the release with those notes and its SLSA provenance, and publishes the npm packages (`npm/build.mjs`).

## Verifying a release

Releases from v1.0.19 on carry SLSA build provenance (`agent86-<tag>.intoto.jsonl`), so you can check that an archive was built by this repository's release workflow with [`slsa-verifier`](https://github.com/slsa-framework/slsa-verifier):

```sh
slsa-verifier verify-artifact <archive> \
  --provenance-path agent86-<tag>.intoto.jsonl \
  --source-uri github.com/Maetek-Technologies/agent86 \
  --source-tag <tag>
```
