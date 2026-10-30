# agent86

An agentic harness on the command line. It connects to remote or local models (OpenAI-compatible providers such as OpenRouter, Anthropic, Ollama, llama.cpp) and lets them use tools and skills: it reads and edits files, runs commands in a sandbox, fetches web pages, talks to MCP servers, and remembers what it learned, with every side effect behind your approval.

```sh
npm install -g agent86
agent86 keys set openrouter          # store a key in the OS keyring
agent86 --model openrouter:<model>   # the interactive interface
agent86 --model openrouter:<model> run "Summarize this repository"
```

To update, run `npm install -g agent86@latest`, or `/update now` inside agent86, which also tells you when a newer release is out.

npm installs the native binary for your platform: Linux x64 and arm64 (glibc 2.35 or newer), macOS on Apple silicon and Intel, and Windows x64. It needs no Python. If you used the earlier Python version, run `pip uninstall agent86` first, since both install an `agent86` command; your `~/.agent86` config, sessions, and keys carry over.

Documentation, releases, and source: [github.com/Maetek-Technologies/agent86](https://github.com/Maetek-Technologies/agent86).
