# Security Policy

## Reporting a vulnerability

Please do not open a public issue for a security problem.

Report it privately, either:

- through GitHub's [private vulnerability reporting](https://github.com/Maetek-Technologies/agent86/security/advisories/new), or
- by email to **tony@maetek.com**.

Include what you found, how to reproduce it, the affected version (`agent86 --version`) and platform, and the impact you expect. You can expect an acknowledgement within a few days. Please give us reasonable time to fix the issue before disclosing it.

## Scope

agent86 runs model-directed tools on your machine, so these areas are of particular interest:

- escapes from the workspace jail, the sandbox executor, or the approval gate
- SSRF or redirect bypasses in `web_fetch`
- leaks of API keys or vault secrets into the model, sessions, traces, logs or errors
- project-config trust bypasses
- MCP, update, or npm-launcher behavior that runs code the user did not approve

## Supported versions

Fixes are made on the latest release.
