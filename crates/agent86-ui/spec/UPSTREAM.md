# Vendored from the A2UI specification

These files are copied unchanged from Google's A2UI repository,
<https://github.com/a2ui-project/a2ui>, at commit
`5ff1b05f02d39f969b52a942e78e864ef3ac01fa` (2026-11-20), and are licensed
under the Apache License 2.0 (`LICENSE-APACHE` here):

| Here | Upstream |
|---|---|
| `v1_0/json/*.json` | `specification/v1_0/json/` (the seven schemas this crate uses) |
| `v1_0/catalogs/basic/catalog.json` | `catalogs/basic/v1/catalog.json` |

The crate's tests carry the specification's test cases the same way, under
`tests/fixtures/v1_0/` (from `specification/v1_0/test/` and
`catalogs/basic/v1/examples/`).

To refresh: copy the same files from a newer upstream commit, update the commit
above, and run `cargo test -p agent86-ui`. The conformance test replays every
upstream case, so a schema change that this crate does not handle shows up as a
failing verdict. Refresh the schemas and the test fixtures together.
