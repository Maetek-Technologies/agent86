# 0006. `ask_user`: the model asks the user its questions through a form agent86 builds

## Status

Accepted — 2026-11-23 (decided with the maintainer the same day, before any code: the name, an ACP fallback for clients without agent86's extensions, and the goal that a form is how the model asks the user anything)

## Context

**The goal.** When the model needs information or a decision from the user, it should ask through a form by default, on every surface where someone can answer. A question asked in prose gets a free-text reply the model may misread, and a form is quicker to answer. Because the form becomes the default, it has to work almost every time, on the models people actually use, including small and cheap ones.

**What exists.** ADR 0005 gave the model `render_ui`, which draws A2UI 1.0 surfaces, forms included. The model writes the A2UI itself: a flat component list linked by ids, bindings into a data model, and a Submit button whose event `context` maps every answer back to a path. A seven-question form is about 4,000 characters of JSON nested six levels deep.

**What failed.** On 2026-11-23, in the terminal interface on `openrouter:anthropic/claude-haiku-5.5` (session `bfd654d1701a`), the user asked for clarifying questions "using a form". All twelve `render_ui` calls, over three turns, failed before the tool ran, and each turn ended with the circuit breaker. Each call was the right form with one or two extra `}` in the same place: the run of closers after the Submit button's `context` map (`{"@path":"/output"}}}}}}]`), where the payload is nested deepest. Dropping every closer that does not match the container it would close turns 10 of the 12 into the intended surface; the other two closed the top-level object early, which cannot be repaired without guessing.

agent86 made it worse in three ways:

- The observation (`invalid JSON in tool arguments: '<first 200 characters>' could not be parsed`, as in Python) shows the payload's beginning, which was fine. The break was 3,400 characters in.
- The failed call was replayed to the model as `{"__invalid_json__": "<raw>"}`, as Python does. The model saw a key it never wrote and concluded, in four turns running, that it had "used the wrong parameter name", while repeating the same mistake.
- A call answered before dispatch (invalid JSON, a refusal by the gate, an unknown tool) records no `tool_call` trace event, as in Python, so `agent86 trace show` listed twelve model calls and a tripped breaker with no reason.

**The measurements so far** (ADR 0005, eleventh amendment) covered five small tasks of at most three fields: `claude-haiku-5.5` was valid on the first try in 3 of 5 and within one correction in 5 of 5. Nothing measured forms of the size a model writes when it asks its clarifying questions. For this ADR the live check gained two task sets (the baseline below):

- `large`: four forms of seven to nine questions, each asked in one message. `claude-haiku-5.5` drew all twelve runs validly on the first try.
- `session`: the failed session as it ran. A turn of clarifying questions in prose over a home folder, with the read-only tools offered, then "Ask me these questions using a form". `claude-haiku-5.5` was valid on the first try in 2 of 6 runs and within one correction in 3 of 6. In the other 3 it never drew the form within the turn, and every failure was invalid JSON.

The same model and the same size of form go from 12 of 12 to 2 of 6 depending only on what came before. Any default built on the model writing A2UI is then only as reliable as the least favorable context it meets, and a check that passes in a clean context says little.

**Why prompt text and repair cannot close the gap.** The model did not misunderstand the tool. It built the right form every time and miscounted closing braces. Better errors, a repair for unmatched closers, and prompt advice all reduce failures, and are worth doing (see "`render_ui`, hardened alongside"). But as long as asking seven questions means writing 4,000 characters of deeply nested JSON, some share of calls fails, most on the cheapest models and in the longest contexts. A default has to be dependable by construction.

## Decision

**Ruling:** agent86 adds `ask_user`, a tool whose arguments are a short, flat list of questions; agent86 builds the form from it, so the form is always valid, draws it on every surface where the user can answer (as an A2UI form, through an ACP extension, or as text), returns the answers keyed by question id, and tells the model, wherever the tool is offered, to ask the user its questions through it instead of in prose.

### The tool

`ask_user`, beyond Python. It is not side-effecting, needs no approval, is not `parallel_safe` (it reaches the user), and always waits for the answer (no `wait` flag).

The model's arguments:

```json
{"title": "Reorganizing your home folder",
 "questions": [
   {"id": "scope", "question": "Which content should the plan cover?", "kind": "choice",
    "options": ["Whole home folder", "Personal content only", "Specific folders I list"]},
   {"id": "goal", "question": "What is the main goal?", "kind": "multi",
    "options": ["Reduce clutter", "Find files faster", "Remove old backups"]},
   {"id": "keep", "question": "Which folders must not move?", "kind": "text", "optional": true}
 ]}
```

- `title`: optional, a heading for the form.
- `questions`: 1 to 10. More than that is refused with a result asking for the most important ten, then a second call.
- Each question has an `id` (an identifier: letters, digits, and `_`, starting with a letter or `_`; unique in the call), the `question` text, a `kind`, and, for the choice kinds, its `options`.
- `kind` is one of:
  - `choice` (exactly one of the options)
  - `multi` (any number of them)
  - `text` (a line)
  - `long_text` (a paragraph)
  - `number`
  - `yes_no`
  - `date` (an ISO 8601 date)
- `options`: up to 12 plain strings, for `choice` and `multi` only (one is enough, since "Other" is added; none makes the question `text`; options on other kinds are ignored, since slice 8). Each option is its own label and value, so there is no label-and-value object to nest.
- Every choice question also offers "Other", with a field for the user's own words. A list of options never traps the user, and the model does not have to remember to add the option.
- `optional`: `false` unless set. A required question must be answered before the form can be sent.

At its deepest the arguments nest four levels (`{"questions":[{"options":["..."]}]}`). The session's seven questions come to about 800 characters, against about 4,000 for the same form in A2UI. The model writes no ids to link, no bindings, no button, and no `context`.

The tool checks the arguments before anything is shown. Each problem is named by the question's index and id (a duplicate id, a choice question with fewer than two options, `options` on a `text` question, an unknown kind), with one line per problem, so a model that gets it wrong can fix it in one more call. The repair of JSON-string arguments (`tools::repair_arguments`) applies as for any tool.

**Strict mode** (deferred to 1.3.0; see slice 6). The schema is small and closed, so it is written to fit providers' strict tool modes, which constrain the model to valid JSON matching the schema. Where the provider supports strict mode, agent86 sends it for this tool. That needs a new `Tool::strict()`, which the adapters pass on in each provider's own form, checked against each provider's documentation when the slice is built. agent86 sends no strict flag today. Strict modes generally want every property required and none added, so the schema sent in strict mode lists every property as required and marks the optional ones nullable. The tool takes a missing property and a `null` the same way, so the providers without strict mode lose nothing.

**The answers** are the tool's result, as one JSON object keyed by question id, after a line the model reads as the answers:

```
The user answered:
{"scope": "Personal content only", "goal": ["Reduce clutter", "Find files faster"], "keep": "AppData, .ssh"}
```

- `choice` answers are the chosen option's text, and `multi` answers a list of them.
- An "Other" answer is the user's own text. The result lists which answers were typed, so the model knows not to look for them among the options.
- `yes_no` answers are `true` or `false`, `number` answers a number, `date` answers `YYYY-MM-DD`, and an unanswered optional question is `null`.
- The answers pass through the ingress guardrail and the vault's redaction, as typed text does.
- If the user closes the form without sending it, the result says so (`ok`, since nothing failed and the circuit breaker must not count it) and tells the model to go on with what it knows, or to end its turn saying what it still needs, and not to ask the same questions again in that turn.
- Escape or Ctrl+C while the form waits cancels the turn at that safe point, as at an approval.

### Where it is offered, and how it is answered

`ask_user` is registered wherever someone can answer, and nowhere else. Every question goes through one hook, a `QuestionPrompt` on the harness, which each surface installs as it installs the approval and secret prompts. Like them it is asked through `ApprovalGate::queue`, one question at a time, and declined once the turn is cancelled. The tool validates the questions, asks the hook, and returns its answer. It never knows which surface it is on.

| Surface | How the questions are answered |
|---|---|
| The terminal interface | A form, built as A2UI (below) and drawn by the interface's agent86UI renderer, in the frame ADR 0005 gave model-drawn surfaces. It stays the form even when `[ui] agent86ui = "off"`, since the model writes none of it. |
| The plain loop (`--plain`, `AGENT86_PLAIN`, piped input with a terminal to answer) | Text, one question at a time, through the same stdin reader the approval prompt uses: numbered options, `multi` answered as a comma-separated list of numbers, anything else taken as the "Other" answer, and Enter skipping an optional question. |
| `sdk` embedders (agent86-desktop) | An embedder that declared a `Ui` with an `on_wait` waiter gets the built form as `TurnEvent::Ui` messages and answers through its waiter, as for a waiting `render_ui` call, so agent86-desktop needs no new code. Without a waiter the tool is not registered. |
| ACP clients that set agent86's questions capability | The questions as a request (below); the client draws its own form and answers. |
| ACP clients with the agent86UI extension (ADR 0005, slice 4) and not the questions capability | The built form, through that extension's blocking wait. |
| Every other ACP client | The text fallback, which ends the turn (below). |
| `run`, CI, piped input with no terminal, sub-agents | Not registered. `run` keeps Python's behavior and output, and a sub-agent reports what it needs to its caller. |

**The built form.** The core turns the questions into one A2UI `createSurface` call in the basic catalog: a Column holding the title, and for each question its text and its input (ChoicePicker, mutually exclusive or multiple selection, for the choice kinds; TextField, with the `longText` or `number` variant where needed; CheckBox for `yes_no`; DateTimeInput for `date`), each bound to `/answers/<id>`, plus the "Other" field and a Submit button whose event `context` binds every answer. It goes through the same redaction, egress guardrail, policy, and validator as `render_ui`, and a test checks every kind against the validator. The form is built by agent86, so a failure to validate is agent86's bug, never the model's: it is reported as an error and recorded, not handed back for the model to fix. The surface id is `ask_<n>`. When the form is answered it is replaced by a copy in scrollback, as an answered `render_ui` surface is.

**The ACP questions extension** follows the namespace rule of the state, memory, and secrets extensions:

- The capability is `com.maetek.agent86/questions`, advertised to all.
- For a client that sets it, `ask_user` sends a `_com.maetek.agent86/questions/ask` request through `TurnSink::call` (`sessionId`, `title`, `questions`, in the tool's own shape). The client answers with `{"answers": {...}}` or `{"dismissed": true}`.
- A cancelled turn declines the request, as it does a permission request.
- It is the simplest path for a client: no A2UI renderer is needed, just a form of its own. agent86-web is its first client.

**The text fallback** is for an ACP client with neither extension. ACP has no way to ask a free-form question in the middle of a turn, so the turn ends there:

1. The tool returns the questions as numbered text, with each choice question's options lettered (`a) Whole home folder  b) Personal content only ...`) and "or say something else" after them.
2. The harness sends that text to the user as the agent's message and records it as an ordinary assistant message after the tool result. The history then alternates as every provider requires, and it reads naturally on resume and in Python.
3. The harness ends the turn after that step without another model call. The ACP stop reason is `end_turn`, and no new `StopReason` value is stored, so `state_json` stays readable by Python.
4. The tool's result tells the model the questions were shown as text and that the user's reply comes as the next message.

The fallback's answers come back as free text, not structured: the price of a client that draws nothing.

### The prompt rule

Wherever `ask_user` is registered, the system message gains a short section (outside `BASE_IDENTITY`, which stays Python's), rebuilt per request like the agent86UI and MCP sections:

> When you need information, a preference, or a decision from the user before you can go on, call `ask_user` with all your questions at once. Do not ask them in your reply. Use `choice` or `multi` with options whenever the likely answers are known; "Other" is always offered. Ask only what you need, then wait for the answers.

When `render_ui` is also offered, its section says that forms which ask the user questions go through `ask_user`, and that `render_ui` is for layouts, choices with custom presentation, and dashboards. The tool's own description repeats the rule, since the model reads it where it writes the call. The exact wording is part of slice 1, settled against the live check.

### The trace

A new kind, `ask_user`, in agent86's namespace (Python has none), recorded after `redact_event`. It holds the question count, the kinds, how the questions were answered (`form`, `extension`, `text`, `fallback`), and the outcome (`answered`, `dismissed`, `cancelled`). The answers themselves are in the session's tool results, not the trace.

### Config

`[ui] ask_user = "auto" | "off"`, default `auto`, meaning registered wherever someone can answer. It goes in `trust::TRUST_FREE`: the default is already on, so an untrusted project can only turn it off. When it is off, the tool and the prompt rule are absent, and the model asks in prose as before. `run` never reads it.

### `render_ui`, hardened alongside

These protect every tool, and `render_ui` on its own, so they are part of this decision:

1. **The invalid-JSON observation says where the JSON broke.** It keeps Python's opening words, then gives the parser's error and character position, the path of containers open there (`messages[0].createSurface.components[16]`), what was expected and what was found, an excerpt ending at the break, and the counts of `{`/`}` and `[`/`]`. Sub-agents use the same function.
2. **A failed call is replayed to the model as `{}`,** not `{"__invalid_json__": ...}`, in the OpenAI-compatible, Anthropic, and Ollama adapters, as the Google adapter already does. Only the request changes; `state_json` keeps the raw text Python keeps.
3. **Unmatched closers are dropped.** Before a call with invalid JSON is answered, agent86 drops every `}` or `]` that does not match the container it would close (outside strings). If that, and only that, gives a JSON object, the call goes ahead with it:
   - The repaired arguments are stored on the call, so replay on resume finds a `render_ui` call it can read.
   - The result notes what was dropped and where, so the model learns.
   - A `tool_args_repaired` event is recorded, as for the string repair.
   - The approval gate sees the repaired call, so nothing runs that the user did not see.

   Any other broken JSON gets the observation from item 1.
4. **A call answered before dispatch is traced.** Invalid JSON, a refusal by the gate, and an unknown tool each record a `tool_call` event with Python's field names, so the trace shows why.
5. **A repeated failure changes the advice.** On a tool's second invalid-JSON call in a row, the observation adds the tool's own advice (a new `Tool::retry_hint`). For `render_ui` that is to build the surface in smaller calls (createSurface with a few components, then updateComponents), or to use `ask_user` for questions.
6. **A Button event's `context` becomes optional in `render_ui`.** If it is left out, agent86 fills it in when the surface is created, binding every top-level key of the surface's data model, so every renderer sees the same action. The prompt's example drops it, which removes the deepest nesting from every form. This goes beyond A2UI and is recorded as an amendment to ADR 0005.
7. **The prompt asks for large surfaces in parts:** createSurface with the first few components, then updateComponents, with `wait` only on the last call.

To check alongside item 6: half the session's retries wrote `{"path": ...}` for `{"@path": ...}` inside `context`. If the validator takes that as a literal object, the action would carry `{"path": "/scope"}` instead of the user's choice. It must either be refused with a message naming `@path`, or be moved to `@path` as `tidy` moves misplaced fields.

### Measuring it

`tests/agent86ui_live.rs` gains `ask_user` checks, opt-in as before (`AGENT86_TEST_UI_LIVE=1`), with `AGENT86_TEST_UI_TASKS` selecting a task set and `AGENT86_TEST_UI_RUNS` repeating each task:

- **Validity:** ten question-asking tasks, at least three runs each, at least half of them in the `session` shape (an earlier prose turn, tools offered), since a clean context hides the failure. The target is that every call is valid on the first try for every model in ADR 0005's hosted table and a local ~27B model.
- **The default:** six requests that need clarification without saying "form" (for example "Plan a better way to organize this folder; ask me clarifying questions first"). This measures how often the model calls `ask_user` rather than asking in prose. The target is at least 9 in 10 on every hosted model. Below that, the prompt rule is revised before the release.
- The large-form `render_ui` set stays as a regression check on the hardening.

**Baseline** (2026-11-23, before any of this, `render_ui` alone, the four large-form tasks three times each, every wait dismissed):

| Model (through OpenRouter) | Task set | Runs | valid first try | valid within one correction | never drew |
|---|---|---|---|---|---|
| `anthropic/claude-haiku-5.5` | `large` | 12 | 12 | 12 | 0 |
| `anthropic/claude-haiku-5.5` | `session` | 6 | 2 | 3 | 3 (each failure invalid JSON, the breaker or the step budget ending the turn) |
| `anthropic/claude-sonnet-5.5` | `session` | 4 | 4 | 4 | 0 |
| `google/gemini-3.8-flash` | `session` | 4 | 1 | 3 | 1 (no `render_ui` call before the step budget) |

`gemini-3.8-flash` failed differently from Haiku: its JSON was valid, but its first calls broke A2UI's rules (a message without `"version": "1.0"`, a `null` in the component list), which the validator caught and the next call fixed. With `ask_user` neither failure can happen, since the model writes neither the envelope nor the components. These runs are few, and the numbers will move between runs. The point they make is the gap between `large` and `session` on the same model.

**After slice 7** (2026-11-23, the `session` set, six runs each, `render_ui` hardened, no `ask_user` yet):

| Model (through OpenRouter) | Runs | valid first try | valid within one correction | never drew |
|---|---|---|---|---|
| `anthropic/claude-haiku-5.5` | 6 | 5 (was 2 of 6) | 5 | 1 (no `render_ui` call before the check's four-step budget) |
| `google/gemini-3.8-flash` | 6 | 3 (was 1 of 4) | 4 | 1 (likewise) |

No run failed on broken JSON. A repaired call counts as valid on the first try, since the user saw the form. Gemini's remaining failures were A2UI's rules (a message without `"version"`), which only `ask_user` removes. The two runs that never drew made no `render_ui` call within the check's four steps, which is a different problem from this one.

**After slice 4** (2026-11-23, `ask_user` offered through the renderer's waiter, every form dismissed):

| Model (through OpenRouter) | Task set | Runs | first through `ask_user` | valid first try | asked in prose |
|---|---|---|---|---|---|
| `anthropic/claude-haiku-5.5` | `ask` | 18 | 18 | 18 | 0 |
| `google/gemini-3.8-flash` | `ask` | 18 | 18 | 17 | 0 |
| `anthropic/claude-haiku-5.5` | `session` | 4 | 4 | 4 | 0 |
| `google/gemini-3.8-flash` | `session` | 4 | 4 | 4 | 0 |

Both models asked through `ask_user` every time, including on requests that never mentioned a form, and neither reached for `render_ui` or asked in prose. The one call that was not valid on the first try was `gemini-3.8-flash` writing an option longer than the 120 characters then allowed, fixed by its next call; the limit is now 200. The `session` replay, which `render_ui` alone drew validly on the first try in 2 of 6 (Haiku) and 1 of 4 (Gemini), went 4 of 4 for both. The remaining measurement (ten tasks, the other hosted models, and a local ~27B model) is slice 8.

**Slice 8** (2026-11-23, through OpenRouter and a local Ollama, every form dismissed). The seven hosted models of ADR 0005's table first ran the `ask` set three times, the `session` replay three times, and the `large` set twice, at the check's old budget of 4 steps a turn:

| Model | `ask`: through `ask_user` / valid first try | `session` | `large` |
|---|---|---|---|
| `anthropic/claude-sonnet-5.5` | 18/18 / 18 | 2/3 / 2 | 8/8 / 8 |
| `anthropic/claude-haiku-5.5` | 18/18 / 18 | 3/3 / 3 | 8/8 / 8 |
| `google/gemini-3.8-flash` | 15/18 / 15 | 3/3 / 3 | 8/8 / 8 |
| `moonshotai/kimi-k3.1-pro` | 18/18 / 18 | 1/3 / 1 | 8/8 / 8 |
| `qwen/qwen3.8-27b` | 15/18 / 12 | 0/3 / 0 | 7/8 / 8 (one through `render_ui`) |
| `openai/gpt-6.2-sol` | 18/18 / 18 | 3/3 / 3 | 8/8 / 8 |
| `deepseek/deepseek-v4.1-flash` | 17/18 / 16 | 0/3 / 0 | 1/8 / 7 (seven through `render_ui`) |

No model asked in prose. Almost every miss was a model that spent its 4 steps looking around the folder (`list_dir`, `find_files`) before it would ask, which the check's budget, not a real session's, cut short. The rest were arguments `ask_user` refused: `questions` sent as something other than a list, options on a text question, a choice with one option, and numbered keys beside a question's fields; and DeepSeek drawing "using a form" requests with `render_ui`. Slice 8 changed four things, then measured again:

- **Leniency where the question is still plain.** Unknown fields are ignored (at the top level and in a question); `type`, `label`/`text`/`prompt`, `choices`, and `required` are taken for `kind`, `question`, `options`, and `optional`; options on a question without them are ignored; one option is a question (with "Other"), and none makes it `text`; an option listed twice is listed once. Too many questions or options, a bad or repeated id, a missing question, and an unknown kind are still refused.
- **`render_ui`'s own description, and the prompt section,** say questions go through `ask_user` "even when the user asks for a form".
- **The check's budget** is `AGENT86_TEST_UI_STEPS` (12 for the sets over a folder), and `AGENT86_TEST_UI_SHOW=1` prints the arguments of each first failing call.

Again, after those changes:

| Model | `ask` x2, 12 steps | `session` x3, 12 steps | `large` x2 |
|---|---|---|---|
| `anthropic/claude-sonnet-5.5` | 12/12 / 12 | 3/3 / 3 | |
| `google/gemini-3.8-flash` | 12/12 / 12 | 3/3 / 3 | |
| `moonshotai/kimi-k3.1-pro` | 12/12 / 12 | 3/3 / 3 | |
| `qwen/qwen3.8-27b` | 12/12 / 11 (a choice with one option, since allowed) | 3/3 / 3 | 8/8 / 8 |
| `deepseek/deepseek-v4.1-flash` | 12/12 / 12 | 2/3 / 2 (one still looking around after 12 steps) | 8/8 / 8 (none through `render_ui`) |
| `ollama:qwen3.5:9b` (local, 9B) | 6/6 / 6 (x1) | 2/2 / 2 (x2) | |
| `ollama:gemma4:12b` (local, 12B) | 4/6 / 4 (x1; twice it answered without asking) | 2/2 / 1 (x2; numbered keys, since ignored) | |

In 104 of these 107 runs the model asked through `ask_user`, and 102 of those 104 calls were valid on the first try; both misses are shapes slice 8 now accepts. Haiku 5.5 and GPT-6.2 were 100% in the first round and were not run again. On every hosted model, no model asked in prose and every call was valid on the first try or would be now. What is left is a model choosing to look around, or to answer, before it asks, which is the model's judgment rather than the form's reliability.

### Slices

1. **Core:** `ask_user`, its argument checks, the answers, the `QuestionPrompt` hook through the prompt queue, the built form and its validator test, the prompt section, the `ask_user` trace kind, and `[ui] ask_user`. Tested with recorded calls against the mock server and a scripted hook. Done 2026-11-23 (`src/ask.rs`, `tools/ask_user.rs`), with these settled as it was built:
   - The hook is `ask::AskPrompt` on `ToolContext::asker` (`ask::Asker`), installed by `Harness::attach_questions`, which also registers the tool. A sub-agent's fork gets an empty `Asker`, and its tool list leaves `ask_user` out.
   - The tool only checks the questions and hands the `ask::Form` to the main loop in its result's metadata (`ask::ASK_KEY`), as a waiting `render_ui` call does. The loop asks (`Harness::ask_questions`, a new `QuestionKind::Ask` in the prompt queue), so the answers pass the vault and the ingress guardrail where typed input does.
   - A hook returns an `ask::Reply`: the `Outcome` (`Answered` with the values by id, or `Dismissed`) and the `Via`. The loop normalizes the values (`Form::accept`), so every surface's answers read the same.
   - `yes_no` is a Yes/No ChoicePicker in the built form, not a CheckBox: a CheckBox starts unchecked, so a required question could not tell "no" from "not answered".
   - Limits: question text 300 characters, the title 120, each option 200 (120 until slice 4's live check, where `gemini-3.8-flash` once wrote a longer option; a refusal costs a round trip, so the limit only keeps forms readable).
2. **The terminal interface:** the hook as the built form, drawn by the agent86UI renderer and answered in place, including "Other", required questions, Escape and Ctrl+C, and the Ctrl+O transcript view. Done 2026-11-23 (`ui::ask_in_form`), with these settled as it was built:
   - Required questions are enforced in the form, not only reported afterwards: Send carries one `@checks` rule (`required` on each required answer, joined with `and`), so the renderer dims it and ignores Enter until they are answered. `Form::accept` still lists any left empty, for surfaces that do not enforce checks.
   - The surface is `agent86_ask_<n>` (`ask::FORM_SURFACE_PREFIX`) and is drawn only by the interface's renderer, never the agent's `UiSession`, so `render_ui` cannot change it and a resumed session does not redraw it. `render_ui` refuses surface ids with that prefix, since a model-drawn surface of the same id would otherwise replace the form on screen. The frame is titled `agent86UI · questions from the agent` instead of the id.
   - The title is plain text, since the terminal renderer does not render Markdown in a Text.
   - `[ui] ask_user` decides; `[ui] agent86ui = "off"` does not turn the form off, since the model writes none of it.
3. **The plain loop:** the text hook. Done 2026-11-23 (`ask_text::ask_in_text`, installed by `repl::plain_loop` when stdin is a terminal, never by `setup::assemble`, which `run` shares): numbered options take a number or the user's own words, an answer that does not fit its kind is asked again, `/skip` alone (or end of input, or Ctrl+C, which cancels the turn) dismisses, and model text goes through `escape::show_controls`.
4. **`sdk`:** the built form through an embedder's `Ui` and `on_wait`, with no new API, so agent86-desktop picks it up from the release. Done 2026-11-23, with these settled as it was built:
   - It is a mode of the loop, not a hook (`Asker::set_draws_form`, set by `Harness::offer_questions_as_forms` from `AgentBuilder::build`): only the loop's `emit` can hand the embedder `TurnEvent::Ui`, so `Harness::ask_questions` draws the form (`createSurface` for `agent86_ask_<n>`), asks the renderer's waiter through the prompt queue, and sends a `deleteSurface` afterwards. A hook, where a surface installed one, still comes first.
   - It is offered only when the `Ui` has `on_wait` and the basic catalog, which the form is built in. `ask::reachable` decides, for the tool and the prompt section alike.
   - The form is never kept in the agent's `UiSession` (`Agent::surfaces` does not list it); an action is taken as the answer only when it names the form's surface, else it reads as a dismissal.
5. **ACP:** the questions extension, the text fallback that ends the turn, and the agent86UI extension's path once ADR 0005's slice 4 lands. They ship together with that slice or before it. Built 2026-11-26, for 1.2.3 (see the amendment below).
6. **Strict mode:** `Tool::strict()` and the adapters, checked against each provider's documentation. Deferred to 1.3.0 (decided with the maintainer, 2026-11-23), for three reasons found when the slice started:
   - **It is not needed for the goal yet.** Without it, `ask_user` was valid on the first try in 35 of 36 live calls (slice 4's measurement), and the one miss was a length limit since raised.
   - **It is not enforced everywhere it is accepted.** Checked against the providers' documentation and by a probe the same day: Anthropic's API takes `strict: true` beside `name` and `input_schema` (no beta header, every current model; every object `additionalProperties: false`, optional properties allowed, `pattern`, `enum`, and `default` supported, `minItems` only 0 or 1, no `maxItems`, `minLength`, or numeric bounds). OpenAI's takes it inside `function` (every property in `required`, optional ones nullable through `"type": [..., "null"]`, every object `additionalProperties: false`, `pattern`, `format`, `minItems`, and `maxItems` supported, `default` not listed). OpenRouter documents `strict` only for `response_format`, not tools; sent on a tool it returned HTTP 200 for `anthropic/claude-haiku-5.5`, `google/gemini-3.8-flash`, and `openai/gpt-6.2-sol`, but Gemini's arguments left out a property the strict schema required, so its endpoint took it as a hint. Each provider also wants its own schema dialect.
   - **It changes `sdk`'s API.** Marking a tool strict needs a field on `ToolSpec`, which `sdk` exports with public fields and without `#[non_exhaustive]`, so adding one breaks a caller that builds it; that is a minor release by the project's rule. 1.3.0 is one already (the Python bindings), so `ToolSpec` becomes `#[non_exhaustive]` and gains `strict` there, with `Tool::strict()`, a schema rewrite per dialect, and strict sent to Anthropic, OpenAI, and OpenRouter.
7. **`render_ui` hardened:** items 1 to 7 above. They are independent of slices 1 to 6 and can land first. Done 2026-11-23, in three parts: 7a, where the JSON broke, `{}` in the replay, and calls answered before dispatch traced; 7b, the closer repair and the advice on a repeat; 7c, the optional `context`, `{"path": ...}` read as `@path` (the check item 6 asked for: A2UI 1.0 does take it as a literal object), and the prompt's rule on building large surfaces in parts.
8. **Measurement and docs:** the live check above, `docs/agent86ui.md` (or a page of its own), the README, the site, and the plan's differences from Python. Done 2026-11-23: the measurement and the leniency it led to are under "Slice 8" above; `docs/agent86ui.md` has an `ask_user` section; the README and FEATURES say it is built; the site's claims wait for the release, as they describe released features.

### The release (settled 2026-11-23)

`ask_user` with slices 1 to 4, 7, and 8 is **1.2.2** (slice 6, strict mode, moved to 1.3.0 the same day). ADR 0005's ACP release moves to **1.2.3** and takes slice 5 with it; scheduled jobs move to 1.2.4 and MCP vault secrets to 1.2.5. None of it adds to `sdk`'s API, so all of it fits patch releases.

### Question still open

1. **An `sdk` hook for embedders without A2UI** (`AgentBuilder::on_questions`). It is left out until an embedder needs it, because it would add to `sdk` and so start a minor release.

## Consequences

- **Asking the user becomes dependable.** The form cannot be invalid, the model's part is short and flat, and strict mode makes even that part valid where providers support it. On every surface with someone to answer, the model uses one tool the same way.
- **The model asks less in prose.** Wherever the tool is offered, answers come back as structured values instead of free text the model has to parse. Under `run` and in CI nothing changes.
- **Every surface needs a way to ask.** The terminal interface and the desktop reuse their agent86UI renderers. The plain loop gets a small text prompt. ACP gets a second extension for clients to implement, and the fallback that ends the turn for those that do not.
- **More prompt.** The section is a few sentences plus the tool's schema, far less than the agent86UI catalog section, and it is present only where the tool is.
- **`render_ui` keeps its place** for anything that is not a list of questions, and gets more robust, as does every tool, from the hardening.
- **Python compatibility is untouched.** No change to `run --json`, `state_json`'s schema, the database, the trace format, or the config Python reads. A session that used `ask_user` opens in Python, which shows an unknown tool's calls and the answers as tool results. Replaying failed calls as `{}` changes only what is sent to the provider.

## Amendment: slice 5, ACP (2026-11-26)

Built as decided. Where it settles what the decision left open:

- **Which path, when the session opens:** the questions extension if the client names it, else agent86's form if the client has the a2ui extension (`Harness::offer_questions_as_forms`), else the text fallback (`Harness::offer_questions_as_text`). `[ui] ask_user = "off"` offers none of them in ACP too, though ACP reads no other `[ui]` setting: it is the user's choice to have the agent ask in prose.
- **The questions extension's request** carries the `Form` in the tool's own shape (`title` only when set; `options` and `optional` only where given) after `sessionId`, with every value the vault holds replaced by its placeholder. An answer is `{answers}`; anything else, an error, or no sink is a dismissal (`Via::Extension`), and a cancel is declined by the queue before the hook is asked.
- **The text fallback:** the tool's result says the questions were shown as text and the reply comes next (`ask::SHOWN_AS_TEXT`), and carries the text (`ask::form_text`, through the vault and the egress guardrail, as the model's own text is) under `ask::ASK_TEXT_KEY`, which the loop takes out. After the step's results the loop emits it as `TurnEvent::Text`, appends it as an assistant message, and ends the turn as a final answer does (phase `done`, `turn_end`, an episode), skipping the verification gate and the stall note: the turn is waiting on the user, not answering. Two calls in one step show both texts. `Via::Text` covers it (`ask_user`'s trace `outcome` is `shown`).
