# Adding secrets from an editor (ACP)

**Status:** options (none chosen, nothing built)
**Last updated:** 2026-11-12

This document lists the ways a user could add a secret to agent86 while working in
an editor over the Agent Client Protocol (`agent86 acp`). It is entirely beyond
Python (Python has no ACP server), so whichever option is built is recorded in the
project plan as a deliberate difference, and in [ACP.md](ACP.md) for users.

## Where things stand

Editor sessions can *use* secrets but cannot *take* one
([ACP.md](ACP.md), "Secrets come from the terminal"; the decision is in
`docs/PROJECT_PLAN.md`, "Secrets over ACP, decided and built 2026-11-05"):

- A secret entered at a terminal in the project (`agent86`, then `/secret NAME`,
  choosing to remember it) is stored in the OS keyring under
  `secret:<md5 of the workspace path>:<NAME>`, and editor sessions that open the same
  folder load it (`HarnessBackend::with_secret_store`, `SecretVault::load_remembered`).
- In the editor, `/secret` lists the names (never the values) and
  `/secret forget NAME` removes one from the session and the keyring.
- `request_secret` is not offered, because there is no prompt hook in `acp`.

A value typed into the editor is never the way in:

- **The protocol forbids it.** ACP's form-mode elicitation must not be used for
  passwords or API keys, and there is no masked field. Credentials obtained through
  URL mode must not be sent back over ACP either.
- **It would leak.** Whatever is typed into the editor's box stays in the editor's
  thread and history, and if it went in as a prompt the model would see it.

Every option below keeps the vault's rule intact: the model never sees a value, and
redaction (`ToolRegistry::dispatch`, `StreamRedactor`, `Recorder`) is unchanged.

## Option 1: `agent86 secret set NAME`, a CLI command

A new non-interactive subcommand, run in the editor's built-in terminal (or any
terminal) from the project folder:

```
agent86 secret set NAME      # hidden prompt for the value
agent86 secret delete NAME
agent86 secret list          # names only
```

- Reads the value hidden with `keys::read_key` (so `rpassword`, or `stty -echo`
  under MSYS, as `keys set` does).
- Checks the name with `vault::valid_name`, refuses `RESERVED_NAMES` and values
  shorter than `MIN_SECRET_CHARS`, exactly as the vault's `accept` does, all before
  anything is written.
- Stores the value under the project's keyring account and adds the name to the
  `secret-index:<hash>` index, so the layout is the one the terminal's "remember
  for this project" writes and every surface reads.

**Pros:** small; reuses existing code; no protocol work; works for every editor;
the same command is useful outside editors (scripts, setup docs).

**Cons:** a session that is already open does not see the new secret until a new
session starts. A follow-up could have `/secret` in the editor reload the index
from the keyring (`load_remembered` again), which is a read, not a value entry.
The user also has to leave the chat for a terminal.

**Tests:** CLI refusals through the binary (bad name, reserved name, short value,
no terminal); the keyring write through a fake `SecretStore`, never the real
keyring, as `keys` tests do.

## Option 2: a one-time page on `127.0.0.1`

`/secret NAME` in the editor starts a short-lived HTTP listener and answers with a
link, for example `http://127.0.0.1:53817/s/<token>`. The user opens it in a
browser, pastes the value into a masked field, and chooses whether to remember it
for the project. The value goes straight from the browser into the running
session's vault (and the keyring when remembered); it never passes through ACP or
the editor's thread.

Safety requirements:

- Bind to `127.0.0.1` only, on a random port.
- A random, single-use token in the path (at least 128 bits); the listener stops
  after one successful submission or a short timeout (a few minutes), whichever
  comes first.
- Check the `Host` header is `127.0.0.1:<port>`, so a DNS-rebinding page cannot
  reach the listener; refuse cross-origin posts (check `Origin`).
- Accept the value only by `POST`; never put it in a URL. Send
  `Cache-Control: no-store` and a strict `Content-Security-Policy`.
- Never log the value; the page's request goes nowhere near the `Recorder`.
- Apply the vault's own checks (`valid_name`, `RESERVED_NAMES`,
  `MIN_SECRET_CHARS`) and show the refusal on the page.
- The editor's message shows only the link and, afterwards, `secret NAME added`.

**Pros:** works inside any editor, with no protocol support needed; the secret is
available to the running session at once.

**Cons:** the most code (a small HTTP server and page, with the checks above);
needs a browser on the same machine, so it does not suit a remote editor session
without port forwarding.

**Tests:** in-process, against the listener: a good submission reaches the vault
and the token stops working; a wrong token, a wrong `Host`, a `GET` with a value,
and an expired listener are refused; nothing in the editor's messages or the
trace holds the value.

## Option 3: option 2, opened by the editor (URL-mode elicitation)

The same listener as option 2, but instead of putting the link in a chat message,
agent86 asks the editor to open it through the protocol's URL-mode elicitation,
for clients that advertise it (`elicitation.url`). The value still goes only to the
local page, which is what URL mode requires. Clients that do not advertise it get
option 2's link in the chat.

**Pros:** smoother where supported: the editor opens the page itself, and the
request is clearly from the agent.

**Cons:** depends on client support. How far URL-mode elicitation has got in the
ACP spec, and which editors support it, has not been checked; do that against the
spec repository's `docs/protocol/v1/` and `schema/v1/schema.json` before building
it, and advertise nothing that is not built.

## Recommendation

Build option 1 first: it is quick, safe, and matches how `agent86 keys set`
already works. Add option 2 if adding a secret without leaving the editor turns
out to matter, and option 3 on top of it once a target editor supports URL-mode
elicitation.
