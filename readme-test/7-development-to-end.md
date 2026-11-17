## Development

Install a current stable Rust toolchain, then run:

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Try it against OpenRouter. Either set `OPENROUTER_API_KEY` in your environment, or store the key once in the OS keyring:

```bash
cargo run -- keys set openrouter      # prompts with input hidden, including in Git Bash
cargo run -- auth                     # interactive: choose a provider, then store its key
```

Then run:

```bash
cargo run -- --model openrouter:<model> run "Say hello"
cargo run -- --model openrouter:<model> run --json "Say hello"
cargo run -- --model openrouter:<model>                  # the interactive REPL
```

Any `[providers.<name>]` block with a `base_url` is treated as OpenAI-compatible; add `api_key_env` if the endpoint needs a key.

OpenRouter models have no built-in price. To see costs, add your model's rates (USD per million tokens) to `.agent86/config.toml`:

```toml
[pricing.models."openrouter:openai/gpt-4o-mini"]
input_per_mtok = 0.15
output_per_mtok = 0.60
```

OpenRouter may serve a model from a host that handles it badly, for example one whose thinking model writes stray tags and then repeats itself (agent86 stops such a loop and asks again). To keep the model off that host, pass OpenRouter's [provider routing](https://openrouter.ai/docs/features/provider-routing) preferences, which are sent as the request's `provider` field:

```toml
[providers.openrouter.routing]
ignore = ["Phala"]
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md). Report security issues as described in [SECURITY.md](SECURITY.md).

## License

MIT, matching the reference implementation. See [LICENSE](LICENSE).
