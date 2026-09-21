# Mimir in Kratos

Kratos uses Mimir's native `mimir acp` stdio server, not an adapter package.

## Setup on the run device

Install the native Mimir CLI and confirm `mimir --version` works. Start `mimir`,
open `/model`, and choose **+ Connect provider** to configure a provider or complete
ChatGPT sign-in. Trust the project/plugins deliberately in Mimir before using ACP.
There is no `mimir login` subcommand or ACP login flow; credentials belong on the
run device, never in chat, forms, executable arguments, or screenshots.

Enable Mimir in **Settings → Agents**, then select it in the existing harness picker.
Discovery checks PATH, the login shell's PATH, `~/.mimir/bin`, and `~/.local/bin`.
If needed, set `MIMIR_EXECUTABLE=/absolute/path/to/mimir` in the Kratos run process's
environment and restart it. This is a binary path, not `mimir acp` or extra flags.
Kratos supplies `acp`; model and effort selection happen through ACP configuration,
not unsupported `mimir acp --model` arguments.

## What appears in the existing UI

| Capability | Support and presentation |
| --- | --- |
| Models and settings | Provider-qualified IDs such as `chatgpt/gpt-5.6-luna`; advertised ACP configuration choices in the existing model/settings pickers. |
| Reasoning | Choices are fetched for the selected model on demand, not guessed for the entire catalog. **Off** is explicit and distinct from leaving effort unspecified; it is offered only when supported. |
| Messages and titles | Public assistant output and ACP session titles use the normal transcript/sidebar. Mimir publishes committed output, not first-token streaming or private reasoning. |
| Commands and plans | Advertised slash commands use the composer. Public Markdown proposals render as expandable documents; delivery steps appear in a compact transcript checklist, separate from the message queue. |
| Tools, shell output, edits | Existing tool cards show semantic titles, original commands, public stdout/stderr, process status and diffs. Output is bounded; an inspection result is not proof that its process exited. |
| Questions | Standard ACP forms use existing input controls: text, single/multiple choices, optional notes and none-of-the-above. Decline/dismissal/cancellation do not invent answers. See live limitation below. |
| Stop and continuation | Normal Stop sends `session/cancel`. Session attachment prefers replay-free `session/resume`; load fallback avoids duplicating saved history. |
| Goals | The negotiated session extension reports actual goal phases and public pause/block explanations. Pausing or waiting for input is not successful completion. |
| Subagents | Negotiated public child identities link canonical launch cards to read-only child transcripts. These are committed public messages/tools, not private reasoning or first-token streaming. |

Models are discovered from Mimir rather than a hard-coded provider catalog.
A listed model is not proof of configured credentials. Selecting a model refreshes
its actual effort/settings choices; unrelated models do not inherit its ladder.
Kratos does not read Mimir's auth files or private session journals.

ACP forms and ordinary approval handling are not a filesystem sandbox. Mimir's
native ACP tools do not pause for per-tool approval round trips. Kratos does not
advertise client-backed execution terminals or filesystem access; shell results
arrive as public tool content, not a second terminal/stdout stream.

## Commands and goal control

- `/plan [task]`: plan work using Mimir's native command.
- `/goal [objective]`: start an autonomous objective; bare `/goal` shows its state.
- `/goal pause`: pause active work. `/goal resume`: continue after the current prompt settles.
- `/goal edit <objective>`: change the goal; `/goal clear`: clear it.
- `/compress`: compress context. `/init [focus]`: create project `AGENTS.md`.

Ordinary prompts and commands are not wrapped or rewritten. Goal start/resume use
standard `session/prompt`, with one tracked model request. Safe goal controls use
the negotiated extension while that request is active, rather than waiting behind
a long-running goal. Normal **Stop** remains standard `session/cancel`.

### Reusable ACP extension

Richer goal controls and child transcripts require Mimir's advertised
`agentCapabilities._meta["mimir.dev/session"]` version **1**, negotiated during
initialization. This follows [ACP extensibility](https://agentclientprotocol.com/protocol/v1/extensibility):
custom data stays in `_meta`, custom methods start with `_`, and unsupported
capabilities do not become pretend UI controls.

- `_mimir/session/state`: current public goal and child identities; the same
  notification carries replacement state when it changes.
- `_mimir/session/goal`: `show`, `pause`, `edit …`, or `clear`, using Mimir's native
  command dispatcher without acquiring another model prompt lease.
- `_mimir/session/child`: a paginated, revisioned snapshot of an attached parent's
  actual public child transcript. Revisions replace snapshots, never duplicate
  messages. Explicit omissions must remain visible.

These are Mimir APIs usable by any ACP client, not Kratos-specific server paths.
Child views are read-only, parent-scoped, and are not independently attachable
`session/load` sessions. No private journals are read by the client.

There is still no ACP plan accept/reject API. The client does not invent approval
buttons or expose native TUI-only screens. Mimir's required server changes are
maintained locally alongside this Kratos integration; capability negotiation,
not the CLI version string, determines availability.

## Verification and remaining limits

Targeted checks (run from the repository root; allow up to 600 seconds each):

```sh
cargo test -p kratos-harness --test mimir --test mimir_lifecycle --locked
cargo test -p kratos-harness --lib acp::elicitation::tests:: --locked
cargo test -p kratos-proto --locked
cargo test -p kratos-engine --lib registry::tests --locked
cargo test -p kratos-ui --lib pickers::tests --locked
cargo build -p kratos --locked
```

The new fixtures cover model/configuration selection, public input round-trips,
exact cancellation, stale/foreign requests, goal interaction, unchanged commands,
and resume/load behavior. Fixtures are protocol checks, not a substitute for live runs.

`cargo run -p kratos-harness --example mimir_probe -- <absolute-disposable-cwd>`
discovers real models/commands without a model prompt. Supplying its optional prompt
starts paid/model-backed work. `mimir_forms_probe` takes `<cwd> <absolute-mimir-binary>
[full|choices]` and uses `chatgpt/gpt-5.6-luna` at medium effort (180-second deadline).
Use an isolated HOME and an existing login-managed `MIMIR_CODING_AGENT_DIR` for
live probes; never copy credentials into fixtures or publish raw session logs.

Live single-choice, multi-select and optional-note round-trips passed. The retained
**full** probe requiring a root free-text question failed; it was not weakened.
Mimir's local schema allows omitted `options`, but its ChatGPT Responses adapter
omits `strict` (ordinary Responses sends `strict:false`). Backend strict
normalization is a hypothesis, **not a proven cause** of that failure. This is
separate from the live-proven concurrent active-goal pause rejection above.

Three unrelated harness failures also reproduce on the untouched baseline:
`windows_extra_candidates_resolve_through_pathext_variants`,
`windows_pathext_environment_reorders_extensions`, and
`models_fall_back_to_the_static_catalog_when_the_probe_fails`.
No tests were disabled or weakened; this is not a claim that the entire suite or
every Mimir capability is live-verified.
