## Summary

<!-- What does this change, and why? Link the issue it addresses. -->

## Compatibility

<!-- Does it touch behavior shared with the Python implementation (CLI output, exit codes, `run --json`, config, session/trace/keyring formats)? If it differs deliberately, note it in docs/PROJECT_PLAN.md. -->

## Testing

<!-- Which tests cover this? No credentials or network in default tests. -->

## Checklist

- [ ] `cargo test --all-targets`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] Docs updated (`docs/PROJECT_PLAN.md`, README) if a stage landed
- [ ] No secrets in code, tests, or logs
