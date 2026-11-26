# Running a local model

agent86 connects to a model server; it does not install or start that server.
This walkthrough uses Ollama on the same computer. Choose a model that fits
your available memory and supports tool calling if you want the agent to use
tools. Running locally avoids a hosted token bill, but hardware, electricity,
and rented GPU time still have costs.

## Start Ollama and download a model

Install [Ollama](https://docs.ollama.com/quickstart) for your platform. Open its
app, or start the server in a separate terminal if it is not already running:

```sh
ollama serve
```

Download the model, then check that it appears in the local model list:

```sh
ollama pull llama3.1:8b
ollama list
```

The download needs disk space and network access. The example model name is
not a memory or performance recommendation; substitute a suitable local model
and use the same name in the command below.

## Connect agent86

With agent86 installed (see the [README](../README.md#install)), run this from
the project you want it to inspect:

```sh
agent86 --model ollama:llama3.1:8b run "List the files in this directory and summarize what is here."
```

Keep `--model` before `run`. The first colon selects the provider; the remaining
`llama3.1:8b` is the model name. The built-in Ollama URL is
`http://localhost:11434`, and a local Ollama model does not need an API key.
For the interactive interface, omit `run` and the goal.

If the connection is refused, check that Ollama is running. If the model is
missing, compare the name with `ollama list`. A text answer alone does not
establish that tool calling works: check that the model actually calls the
requested directory tool. Keep the normal approval prompts for side effects.

## Choose the context window

A larger context needs more memory. In `~/.agent86/config.toml`, an explicit
request sets the window sent to Ollama:

```toml
[providers.ollama]
num_ctx = 8192
```

Without `num_ctx`, agent86 uses a positive `num_ctx` from the model's Modelfile
as specified. Otherwise it uses the model's trained context length, capped by
`num_ctx_max` (32768 by default). For example:

```toml
[providers.ollama]
num_ctx_max = 16384
```

The cap applies to the trained-length fallback, not to a Modelfile's chosen
window or an explicit `num_ctx`. If discovery cannot determine a window,
agent86 falls back to 8192. See [FEATURES.md](FEATURES.md) for the context and
compaction behavior, and [CONFIGURATION.md](CONFIGURATION.md) for config
precedence. Remove an explicit `num_ctx` when you want automatic discovery.

## Connect to vLLM instead

Install and start vLLM for your hardware using its
[installation guide](https://docs.vllm.ai/en/latest/getting_started/installation/).
For tool use, its server needs `--enable-auto-tool-choice` and a
`--tool-call-parser` that matches the model. Follow the model-specific examples
in [vLLM's tool calling guide](https://docs.vllm.ai/en/latest/features/tool_calling/);
there is no parser name that works for every model.

The built-in `vllm:` provider connects to `http://localhost:8000/v1`. Use the
model ID served by that server after the prefix (replace the placeholder):

```sh
agent86 --model vllm:YOUR_SERVED_MODEL_ID run "List the files in this directory."
```

Its key is optional unless the server requires one. Set `VLLM_API_KEY` through
your normal secret environment, or use `agent86 keys set vllm`; do not write a
key into TOML. agent86 discovers the server's `max_model_len` from its models
response. Ollama's `num_ctx` settings do not configure a vLLM server.

For a server on another machine, see the named `adapter = "vllm"` example in
[CONFIGURATION.md](CONFIGURATION.md#providers-and-servers). Use an authenticated,
trusted connection rather than exposing a development server publicly. A
local provider's token cost display does not include the remote machine's bill.

llama.cpp and LM Studio are also supported; their model prefixes, keys and
server configuration are listed in [CONFIGURATION.md](CONFIGURATION.md).
