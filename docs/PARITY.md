# Parity checklist

Status of the native rewrite against `docs/research/feature-inventory.md`
(§1–§8), audited against the tree after M6. Legend: **done** (implemented and
tested), **partial** (core in place, listed gaps), **deferred** (intentionally
not built yet).

## §1 Desktop app

| Item | Status | Notes |
| --- | --- | --- |
| 1.1 Window shell | partial | gpui window, always-dark theme, external links via OS browser. Deferred: frameless-inset/traffic-light chrome (macOS packaging not executed), single-instance lock, dev-vs-packaged port split (env vars instead). |
| 1.2 App phases | done | Local-ready / pairing-required / app phases with crossfade; boot splash with fade-out cap (`ui/src/shell.rs`). |
| 1.3 Shell layout | done | Collapsible drag-resizable sidebar (208–400), right Changes pane (360–760, 52% cap), header variants, widths persisted to `ui-settings.json`. |
| 1.4 Keyboard shortcuts | done | Customizable keymap, click-to-record with conflict detection, per-row reset, rows grouped into Panels / Sessions / Jump cards (`ui/src/settings/shortcuts.rs`); persisted with UI settings. Session stepping is Ctrl+Tab / Ctrl+Shift+Tab over the sidebar list (rebindable) rather than the original's Shift+Up/Down. Thread jumps are Mod+1–9 over the same list with hold-modifier key-cap hints (t3code `THREAD_JUMP_KEYBINDING_COMMANDS`), and Mod+Shift+A archives the open session. |
| 1.5 Routes | partial | Native navigation instead of URL routes; devices / agents / shortcuts / archived settings pages exist. Profile page (heatmap) is an §8 exclusion. |
| 1.6 Sidebar | done | Device switcher, new session, grouped-by-project or flat, status dots (staleness-checked), row context menu (rename/archive/delete), resort glide. |
| 1.7 Composer | done | Send/Steer/Stop morph, compact↔expanded flip, per-chat drafts, optimistic echo with failure return-to-draft, QuestionPanel (paged, auto-advance, number keys), all four pickers (harness/model, traits, repo with folder browser + clone/create, branch with worktree toggle), image attachments (paste/drop/picker → strip → chunked upload to host device → `withAttachments` refs in prompt text + inline image blocks for the Claude harness; per-chat stash, failure hand-back, lightbox — `ui/src/attachments.rs`). |
| 1.8 Transcript | done | Doc-projection source, virtualized, markdown + syntax highlight, tool folding (ToolGroup/ToolChip), input/error chips, stick-to-bottom band, MessageRail minimap (hover preview, hidden < 48rem), user-bubble attachment thumbnails (112×80, read-back from owning device, 2s→15s retry ladder, seeded cache, click-to-expand lightbox). Generated assistant images use structured doc parts, the same owner-targeted attachment cache/retry/lightbox, and bounded raster decoding. |
| 1.9 Accounts settings | done | Provider cards, usage meters with 80/95% thresholds + reset time, Switch/Forget, paste-code and browser-poll add flows, device switcher (`targetDeviceId`). |
| 1.10 Terminal panel | done | Session-scoped tabs, drag-reorder, middle-click close, height drag, replay-then-tail streams, input coalescing, ANSI emulator (`ui/src/terminal/`). |
| 1.11 Changes viewer | done | Patch → file/hunk/line rows, per-file collapse, ±gutters, time-sliced highlighting, preparing/clean/error states, checkout_id → device+cwd resolution. |
| 1.12 Motion catalog | partial | Motion kit (cubic-bezier curves, fade-in/quick, splash-out, pulse/gradient spinners, menu/dialog-in, resort glide). Gap: prefers-reduced-motion switch. |
| 1.13 State & connection | done | All subscriptions (AuthStatus, WatchDevices/Chats/Sessions/CheckoutDiffs, per-chat WatchDocMessages, LocalDevice probe); reconnect from scratch. |

## §2 Control plane

| Item | Status | Notes |
| --- | --- | --- |
| ListHarnesses / ListModels | done | Relay-forwardable. |
| Run/Subscribe/Interrupt/Steer/RespondInput RPCs | done (changed shape) | Deliberate redesign: these ride the durable doc command queue (`QueueCommand {run|steer|interrupt|respondInput}`) instead of device-addressed RPCs — same capability, offline-tolerant. |
| Repos/folders/worktrees RPCs | done | All eight methods, relay-forwardable. |
| Uploads / ReadAttachmentChunk | done | Chunked staging with persisted progress, path-jailed reads, targeted host delivery, and profile-scoped peer custody for offline recovery. |
| Terminals RPCs | done | Open/Subscribe/Write/Resize/Close, forwardable. |
| Agent-account RPCs | done | Full login/activate/forget/poll surface, forwardable. |
| LocalDevice | done | `{deviceId}`; IPC-only (never forwarded). |
| DataRpc watches + QueueCommand | done | — |
| Mutate ops | partial | createChat/renameChat/setChatArchived/deleteChat/renameDevice done; markChatSeen accepted as a no-op (unseen markers UI-local); `SetChatConfig` exists on the doc layer but is not yet exposed as a Mutate op. |
| AuthRpc | done | IPC-only `AuthStatus`, peer initialization/pairing/invitations, trusted-device list/revoke/status, and disconnect. Pairing state becomes active only after engine restart. |
| Wire types | done | `kratos-proto`: AgentEvent, ToolCall kinds, models/options, entities, AuthState. |

## §3 Backend engine

| Item | Status | Notes |
| --- | --- | --- |
| 3.1 Lifecycle | partial | Profile-scoped device registration, ephemeral presence, stale-session recovery, host-only doc executor with steer→new-turn fallback, and single-instance data-dir lock. `kratos peer init` / `kratos pair` update saved trust while the engine is stopped; daemon install/start/stop/restart/status/uninstall manages launchd or systemd user units. Gaps: login-shell PATH capture for the headed app, crash shield, parent-PID watchdog. |
| 3.2 Sessions engine | partial | Run journal on disk with crash recovery (aborted stamps), steering mailbox at step boundaries, doc hooks at boundaries, streamed part folding at STREAM_COMMIT_MS. Gaps: idle reaper + 10-min stall watchdog for persistent harness sessions. |
| 3.3 Session-docs host | done | SQLite snapshots + processed-command ledger, mark-before-execute, durable peer join per open chat, diff sidecar publish, and cold-chat command delivery through durable nudges. Gap (minor): no boot-time warm-open of recent chats (14d/30). |
| 3.4 Terminals | done | PTYs, 1MB bounded replay + `afterSeq` resume, 32 max, exited 30-min TTL, live shells survive detach. |
| 3.5 Repos/diffs | done | list/add/clone/create, branches, worktrees, checkout identity; CheckoutDiffSync (fs watchers + repair pass, name-status+numstat+patch incl. untracked, 3MiB cap, sha256, sidecar publish); chat.branch upkeep from HEAD watch; folder listing with timeout. |
| 3.7 Pairing / uploads / accounts / device relay | done | One-use invitations, persistent Ed25519 device keys, proof-of-possession authentication, profile-bound tokens, revocation, chunked resumable uploads with peer custody, agent credential switching/usage probes/OAuth, virtual relay sockets, and peer link caching. |

## §4 Harness

| Item | Status | Notes |
| --- | --- | --- |
| Claude Code adapter | done | stream-json, model discovery/effort ladders, AskUserQuestion → requestInput, steering via persistent input, init dedup, subagent filtering. **Live-verified against the real `claude` CLI 2.1.215**: doc-queued run → host executor → subprocess → streamed reply landed complete in the doc. |
| Codex adapter | done | `codex app-server` JSON-RPC (thread/start/resume, sandbox policy); `imageGeneration` lifecycle and inline generated images from `savedPath` (PNG/JPEG/WebP/GIF, 24 MiB), imported atomically into profile uploads. Inline `result` and SVG are deliberately unsupported. |
| Cursor (ACP) | done | Shared `AcpHarness` spec; `cursor-agent acp` (Cursor's native ACP server), turn-boundary steering, no effort ladder (effort rides the model id's bracket suffix). Cursor's blocking extension methods are answered: `cursor/ask_question` → requestInput, `cursor/create_plan` auto-accepts, `cursor/update_todos` → todo chip. **Live-verified against the real `cursor-agent` CLI**: model discovery, streamed reply, todo chip and exec tool calls. |
| Devin (ACP) | done | Shared `AcpHarness` spec; `devin acp` (Cognition's native ACP server), turn-boundary steering, no effort ladder (effort rides the advertised model ids). Unattended mode via the `bypass` mode value. Subagent lifecycle and nested transcripts ride Devin's `cognition.ai/subagentSupport` extension. **Live-verified against the real `devin` CLI 3000.6.14** (2026-09-04): model discovery (193 rows), an edit + `cargo test` turn settled on the truthful `end_turn`, `run_subagent` rendered as a link chip with a nested child transcript, stop → `session/cancel` → `stopReason: cancelled`, and `session/load` resume. |
| Grok (ACP) | done | Shared `AcpHarness` spec; `grok agent stdio`, turn-boundary steering. |
| Hermes (ACP) | done | Shared `AcpHarness` spec; `hermes acp` (Nous Research's native ACP server), turn-boundary steering, no effort ladder yet. |
| Pi (ACP) | done | Shared `AcpHarness` spec; community `pi-acp` adapter (pinned 0.0.33, npx fallback), turn-boundary steering, minimal→max thinking ladder. |
| Mock harness | done | Scripted event replay; powers tests + the e2e smoke. |

## §5 Session doc schema

| Item | Status | Notes |
| --- | --- | --- |
| Containers (meta/messages/commands), LoroText bodies | done | Shape-compatible with TS `packages/session-doc`; `tokens` dropped per §8. |
| Command rules (append-only, host outcome writer, evaluateCommand) | done | Processed-ledger dedupe, TTL, supersede rules. |
| Continuation splitting / joining (MSG_INLINE_MAX 256KB) | done | `split at part boundaries`, `root#cN`, render-time join. |
| Render-parts privacy policy | done | WriteFile content / Edit bodies / etc. stripped; full inputs only in the host journal. |
| Sidecars (tail, diff) + constants | done | — |

## §6 Durable peer

| Item | Status | Notes |
| --- | --- | --- |
| Connectivity | done | The packaged, supervised Tailcat adapter supplies private paths; direct UDP is opportunistic and DERP relay operation is supported. |
| Application authorization | done | One-use pairing invitations, device-key proof of possession, profile-bound tokens, route authorization, and immediate revocation. A Tailcat address alone grants no access. |
| Durable sync/storage | done | SQLite-backed chat update logs, checkpoints, registry rows, presence, sidecars, attachment custody, replay dedupe, offline catch-up, and consistent backups. |
| Device relay | done | Stable byte frames, one current host connection, targeted RPC, durable nudges, host replacement, and explicit local-IPC versus remote-RPC permissions. |

## §7 Deployment boundary

| Item | Status | Notes |
| --- | --- | --- |
| Always-on operation | done | Run the Rust peer on an owner-controlled always-on installation when devices must catch up without overlapping online. Tailcat is connectivity, not the application database. |
| Hosted application backend | dropped | No WorkOS, Cloudflare Worker, Durable Object, R2, Postgres, or signaling service is required by production. |

## §8 Exclusions

| Item | Status | Notes |
| --- | --- | --- |
| Token-usage display dropped | done | No WatchUsage, no doc `tokens`, no profile heatmap; rate-limit meters + Usage AgentEvent passthrough kept as specified. |

## Deferred (cross-cutting)

- **Mobile platform execution** — iOS native bridge and storage code are present; App Store/device validation remains platform-gated.
- **Content encryption beyond transport** — Tailcat/WireGuard encrypts traffic between
  nodes and application authentication authorizes every peer route. Data is not
  additionally encrypted from the owner-operated durable peer or its local storage.
- **macOS packaging execution** — config + steps in `dist/` only (needs a Mac).
- **Engine hardening**: single-instance lock, parent-PID watchdog, crash
  shield, idle reaper / stall watchdog, boot warm-open of recent chats.

## Summary

Table rows above: **40 done · 6 partial**, plus the cross-cutting deferrals
(mobile platform execution, content encryption beyond transport, macOS packaging execution, engine hardening) — the last
overlaps the named gaps in the partial rows.
