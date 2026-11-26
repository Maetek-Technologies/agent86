# Configuring agent86

agent86 reads layered TOML: its built-in defaults, then `~/.agent86/config.toml` (yours), then `./.agent86/config.toml` (the project's), then the `AGENT86_MODEL` environment variable, then `--model`. `agent86 config path` shows which files are read and `agent86 config show` prints the result. A project's file is not trusted until you say so; see [FEATURES.md](FEATURES.md) for what that covers.

## Keys

A provider's key comes from its environment variable or, failing that, the OS keyring. Store one with `agent86 keys set <provider>` (the input is hidden) or pick a provider with `agent86 auth`. Keys never go in config files.

| Provider | `--model` prefix | Key |
| --- | --- | --- |
| Anthropic | `anthropic:` | `ANTHROPIC_API_KEY` |
| OpenAI | `openai:` | `OPENAI_API_KEY` |
| OpenRouter | `openrouter:` | `OPENROUTER_API_KEY` |
| Google (Gemini) | `google:` | `GEMINI_API_KEY` |
| Groq | `groq:` | `GROQ_API_KEY` |
| Ollama | `ollama:` | none (local) |
| llama.cpp | `llamacpp:` | none (local) |
| vLLM | `vllm:` | `VLLM_API_KEY`, only if the server wants one |
| LM Studio | `lmstudio:` | `LM_API_TOKEN`, only if the server wants one |

`agent86 models` lists the providers and where each one's key comes from.

## Providers and servers

Any `[providers.<name>]` block with a `base_url` is treated as OpenAI-compatible; add `api_key_env` if the endpoint needs a key.

A vLLM server elsewhere, a GPU box on your network for example, is a section with `adapter = "vllm"` (beyond Python), and an LM Studio server shared on your network ("Serve on Local Network") one with `adapter = "lmstudio"`. It then behaves like the built-in `vllm` provider: its key is optional, its window comes from the server, and it is priced as local. Its key lives under its own name (`agent86 keys set gpu-box`). vLLM answers tool calls only when started with `--enable-auto-tool-choice` and a `--tool-call-parser` that matches the model.

```toml
[providers.gpu-box]
adapter = "vllm"
base_url = "http://gpu-box.lan:8000/v1"
api_key_env = "GPU_BOX_API_KEY"   # only if the server was started with --api-key
```

## Prices

OpenRouter models have no built-in price. To see costs, add your model's rates (USD per million tokens) to `.agent86/config.toml`:

```toml
[pricing.models."openrouter:openai/gpt-4o-mini"]
input_per_mtok = 0.15
output_per_mtok = 0.60
```

## OpenRouter routing

OpenRouter may serve a model from a host that handles it badly, for example one whose thinking model writes stray tags and then repeats itself (agent86 stops such a loop and asks again). To keep the model off that host, pass OpenRouter's [provider routing](https://openrouter.ai/docs/features/provider-routing) preferences, which are sent as the request's `provider` field:

```toml
[providers.openrouter.routing]
ignore = ["Phala"]
```

## Temperature

agent86 sends `temperature` 0.0 to every model, as the Python version did. A provider section can set its own, from 0 to 2, for a model whose vendor recommends sampling at a higher temperature (many thinking models do):

```toml
[providers.vllm]
temperature = 0.6
```

A specialist sub-agent can set its own in its frontmatter (`temperature: 1.0`), which wins over its model's section ([subagents.md](subagents.md)). Summaries always use 0.0. Anthropic's API takes 0 to 1, so a higher value is sent as 1; Gemini takes no sampling settings, so the setting does nothing there, and neither does it for a model that refuses sampling parameters. Like every provider setting, it needs a project to be trusted when it is set in the project's file.
