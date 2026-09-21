# Tailcat replacement: architecture and verification

## Design

Tailcat is connectivity, not a sync server or account system. The inspected upstream
checkout is `tailscale/tailcat` commit
`fd101889796a947ac514e9d86ec731af2965fad3` (`v0.6.0-6-gfd1018897`).
See <https://tailscale.com/tailcat>, upstream `tailcat.go`, `listen.go`, `README.md`,
and `SECURITY.md`.

An application-owned, pinned Tailcat adapter connects engines to a durable Rust
peer. The working chat2/registry protocols, Loro session model, SQLite snapshots,
outbox, processed-command ledger and run journals remain. HTTP/WebSocket frames
travel over Tailcat, not to the old Worker. No Cloudflare service is required.

A designated always-on peer accepts updates and attachments while an owning engine
is offline. It stores data but never takes agent execution authority from that
engine. Local-only profiles never attach to its transport.

Pairing uses persistent device keys, short-lived single-use invitations, proof of
key possession, profile-bound admission and live revocation. Tailcat addresses
contain secret key material and are not application authorization. IPC-only methods
remain unavailable remotely.

Tailcat exposes Go `Server.Listen`, `Client.DialTCPPort` and persistent keys. Its
allowlist only supports additions, not revocation of existing peers; Kratos handles
application revocation. Public DERP has rate limits, metadata logging, no SLA and
potentially revocable access. Direct UDP is opportunistic; relay-only operation and
an owned HTTPS DERP map are supported. iOS uses an embedded native bridge because
upstream has no native iOS package. Users do not manually run Tailcat commands.

## Fresh-start policy

Creating a peer initializes a new paired profile. Joining an existing peer loads
that peer's data, not this installation's local history. Desktop pairing offers a
runtime switch, with no local-work import choice or progress flow. Existing local
and retired cloud-account files remain untouched on disk.

There is no legacy archive migration CLI, account/device adoption API, old uploads
alias, or automatic old workspace/session import. Normal schema initialization,
current-profile snapshot loading, offline catch-up, checkpoints, journals, and
backup/restore of the new peer remain: recovery is not cross-profile migration.

## Capability-to-replacement checklist

| Responsibility | Replacement | Verification |
| --- | --- | --- |
| Identity and device admission | Profile-bound keys, expiring invitations and revocation | Pairing/proof replay/expiry, wrong profile, restart, active socket closure |
| Targeted remote RPC and presence | Authenticated device relay and durable nudges | Device routing, IPC-only denial, terminals/repos/worktrees/files |
| Sessions and streaming | Durable chat log/checkpoints, retained Loro model | Real Tailcat convergence, causal validation, restart/replay |
| Workspace registry and spaces | Durable HLC rows plus ephemeral presence | Concurrent writes, resync, profile isolation |
| Commands and message queue | Existing host-only execution and processed ledger | Send/steer/interrupt/input, lease contention/edit/remove/reorder, dedup |
| Attachments and pending references | Profile-local staging and peer custody | Chunk resumption, target authorization, sender-offline recovery, digest |
| Tool output/diff sidecars | Profile-scoped peer blob storage | Size/path authorization, restart, full content reads |
| Disaster recovery | Complete new-peer SQLite/key backup generations | Offline restore drill, inventory/digest/permission checks |
| Previews | Authenticated catalogs and Mux bytes over the peer | Large HTTP/WebSocket relay, containment, revocation |
| Profile and UI isolation | Separate local/paired roots, no data carryover | Fresh-start and no-import regressions; UI state stays local |
| Desktop/headless/iOS setup | Managed bundled adapter/native bridge, pairing UI/CLI | Linux native run, platform-specific checks and explicit limits |
| Release distribution | GitHub manifests, checksummed artifacts and installer | Installer, safe updater extraction/rollback, license packaging |

## Verification record

After the PR recovery fixes, the parent ran the combined engine/doc/rpc/sync/
preview/update/CLI suite with `--locked`, `kratos-sync/mock-server`, and serial
execution: **600 passed**, no failures, four existing credential/private-fixture
tests ignored. Log: `target/verification/pr8-final-integrated.log`. Subsequent
backup-publication changes passed the focused peer-backup and CLI backup tests.
The built binary rejects `kratos migrate`; current peer backup/restore help remains.

Fresh-start regressions verify old/global/local upload roots stay inaccessible
from a new paired profile while source files remain untouched, removed import
RPCs return UnknownMethod, and desktop setup never offers or performs local import.
Focused native UI pairing, device management, keyboard and switch tests passed.
iOS has matching no-adoption and saved-key recovery XCTests; native execution uses
the GitHub-hosted simulator job rather than claiming XCTest execution on Linux.

The retained real Tailcat integration uses the actual Go adapter and a local
DERP/STUN fixture with Rust Auth and EngineRuntimes. It tests pairing, registry/chat
convergence, simultaneous local/targeted-RPC edit contention, queue operations,
wrong-profile denial and live revocation. It stops the executor and durable peer
transports, verifies old endpoints are unavailable, reopens the same identities,
recovers queue/lease state and rejects replay of an accepted command without
another completed execution. This is an orderly restart, not a power-loss test.

Attachment begin/chunk/commit traverses Tailcat; sender engine and Auth adapter
stop before the target fetches with its own bearer. Exact bytes and SHA-256 match.
The authenticated preview integration relays 4 MiB HTTP and 256 KiB WebSocket
traffic and checks profile isolation and revocation.

Installer tests, 34 landing tests, Windows updater cross-compilation and
Linux-hosted PowerShell/archive fixtures passed during the replacement. The pinned
license bundle regenerates byte-identically. Linux app/browser builds use real
WebKitGTK/JSON-GLib. Actual pairing/Devices screenshots were inspected and GPUI
keyboard/management tests passed. See [native verification](testing-environment.md).

Requested Astra low-reasoning UI and code reviews found and verified fixes for
checkpoint data loss, revocation races, re-pairing, adapter recovery and failed
startup cleanup. The fresh-start removal also passed focused Astra low-reasoning
UI/iOS and final integrated backend/code reviews with no outstanding findings.


The PR follow-up added regressions for sender restart before/during attachment
custody upload, revoked-device re-pairing, concurrent identity publication, and
failed iOS redemption preserving a saved key. Native CI exercises macOS/Windows
Tailcat transport and durable-peer recovery; macOS also runs managed Auth tests.
The obsolete edge coordinator job was removed, while its replacement authenticated
preview coverage remains. Native browser fixtures retain their input-isolation
assertions and wait for actual popup teardown rather than fixed timing.

## Operational/platform limits

- Native macOS/Windows jobs and iOS simulator tests run in GitHub Actions. See
  [PR #8 checks](https://github.com/wasimysaid/Kratos/pull/8/checks) for the exact
  tested commits and current outcomes; source review and cross-builds are not
  substitutes. Physical-device, power-loss, and release signing/notarization
  validation remain separate rollout checks.
- No live cloud data, local historical files, deployments or releases were deleted
  or published. Starting fresh does not authorize destructive cleanup. There is no
  supported old-data import workflow. New-peer [backup/restore](peer-backup.md)
  remains available; follow [release retirement](release-distribution.md) for old
  updater clients and publish new installer/adapter assets before rollout.
- Non-overlapping device availability requires a running peer. Public DERP has no
  SLA; owned relay configuration is supported. Protect peer data and backups with
  operator-controlled filesystem encryption and access controls: encrypted
  transport is not encryption at rest.
