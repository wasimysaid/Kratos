# Kratos architecture

Kratos is a native multi-device controller for coding agents. Every installation runs the Rust engine; the desktop and iOS applications are viewports over typed RPC. A fresh installation is local-only and needs no account or network service.

## Runtime topology

```text
headed UI ── local IPC/in-memory RPC ── engine ── local stores and run journals
                                           │
                                           └── managed Tailcat adapter ── durable peer
                                                                           ├── other engines
                                                                           └── iOS viewport
```

Tailcat supplies private connectivity only. Kratos supplies application authentication, authorization, durable synchronization, storage, and RPC framing. The application starts and supervises the pinned `kratos-tailcat` adapter; users do not run Tailcat commands manually.

One installation initializes the profile's durable peer. Keep that installation running on an always-on machine when devices must catch up without overlapping online. Other installations join with short-lived, one-use invitations. Persistent Ed25519 device keys, proof-of-possession challenges, profile-bound bearer tokens, and immediate revocation protect every application route. A Tailcat address is secret but is not accepted as authorization by itself.

The engine exposes only the existing remotely permitted `ControlRpc` surface to paired devices. IPC-only operations remain unavailable remotely. The host device retains authority over its terminals, repositories, worktrees, workspace files, previews, and agent execution.

## Profiles and storage boundaries

The engine resolves one immutable profile at startup:

- **Local**: `profiles/local/`; no peer transport is attached.
- **Paired**: `profiles/<opaque-profile-uuid>/`; transport connects to that profile's durable peer.

Creating a peer starts a fresh paired profile. Joining a peer loads only that peer's existing data. There is no local-work import, legacy-account adoption, or cloud archive migration. Existing local and retired account files are left untouched and are never read into the paired profile. Sessions, registry snapshots, outboxes, processed-command ledgers, run journals, and uploads are profile-scoped. Repository registrations, worktrees, agent credentials, and UI settings remain device-scoped. UI tabs and other intentionally local viewport state are not synchronized.

Changing pairing state does not mutate a running engine's stores. Desktop setup offers a switch that replaces the runtime in-app, with quit/reopen as a failure fallback. Headless users restart the engine after initializing, pairing, or signing out.

## Durable data plane

### Session documents and commands

Each chat is a Loro document containing transcript entries and the shared durable command/message queue. The existing chat2 row protocol provides append-only update rows, batch deduplication, cursor catch-up, causal checkpoint/frontier validation, and resumable checkpoint reads. Devices retain SQLite snapshots and pending-update outboxes.

The chat's owning engine is the only command executor and outcome writer. It marks commands processed before execution, preserving crash/replay idempotence. Send, steer, interrupt, input responses, queue editing, reordering, removal, leases, send-now, and steer-now remain durable document operations. Reconnect does not grant another device execution authority.

### Workspace registry

A profile registry stores devices, spaces, chats, session status, and checkout-diff summaries as conflict-resolved rows. The durable peer persists those rows and relays ephemeral presence. Writer discipline remains device/host based: devices write their own liveness, hosts write their chats and status, and user actions write the permitted LWW metadata or tombstones.

### Attachments, blobs, and sidecars

Uploads are staged in the active profile's local upload directory. Pending references are delivered in chunks, with persisted progress and resumption, to the host and durable peer as required. The peer stores profile-scoped attachment custody, checkpoints, tool-output blobs, and preview/diff sidecars in SQLite-backed storage. Attachment reads are confined to current-profile roots; historical global/account upload directories are not adopted.

### Backups

The durable peer creates consistent periodic recovery generations under its private peer directory. `kratos peer backup` and `kratos peer restore` support recovery of the new peer's data, trust database and transport identity; see `docs/peer-backup.md`. Backup/restore is not an old-backend or local-profile import path.

## Engine and UI composition

- `crates/doc`: Loro session schema, registry model, transcript folding, command and queue rules.
- `crates/sync`: local `DocsStore`, active chat/registry clients, and durable peer HTTP/WebSocket/storage implementation.
- `crates/engine`: sessions, journals, doc/workspace hosts, auth and pairing, Tailcat lifecycle, uploads, previews, repositories, terminals, and RPC dispatch.
- `crates/rpc`: typed local/remote RPC framing and targeted virtual links.
- `crates/harness`: Claude Code, Codex, Cursor, Devin, Grok, Hermes, Pi, and Mimir adapters.
- `crates/ui`: gpui desktop viewport; browser, transcript, queue, composer, terminal, diff, and device/pairing surfaces.
- `apps/ios`: SwiftUI viewport with the native Tailcat bridge and profile-scoped offline cache.
- `apps/kratos`: headed/headless binary and peer, backup/restore, daemon, installer, and updater CLI.
- `connectivity/tailcat`: pinned application adapter and native packaging sources.

The headed application first probes localhost IPC. It attaches to an existing daemon when available; otherwise it embeds an engine and serves the same typed protocol. `kratos headless` runs the same engine without a viewport.

## Connectivity and deployment

The packaged adapter is adjacent to the executable. Source/development runs may set `KRATOS_TAILCAT_ADAPTER` to an explicitly built adapter. Public DERP relays are rate-limited, log connection metadata, and have no availability guarantee; production operators who require durable relay service should pass an owned HTTPS DERP map when initializing the peer. Direct UDP remains opportunistic and relay-only operation is supported.

Static landing content deploys with GitHub Pages. Installers, manifests, and update artifacts are GitHub Release assets. The runtime has no hosted application backend and requires no provider bearer or deployment credentials.

## Trust boundaries

Paired devices are trusted for the remotely permitted workspace surface. The owning engine still enforces workspace-relative paths, containment, symlink handling, write conflicts, and the `.git` exclusion. When ignored-file access is enabled, gitignored files such as `.env` may be read remotely; hiding them in a UI is not an authorization control.

The durable peer is profile-isolated and validates the authenticated device on every route. Revocation closes active sync, RPC, preview, and blob access. Local IPC and remote RPC remain separate permission boundaries.
