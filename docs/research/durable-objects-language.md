# Historical Durable Objects language decision

## Status

Superseded by the Tailcat durable-peer architecture in `ARCHITECTURE.md` and `docs/tailcat-replacement.md`.

This note originally compared TypeScript and Rust implementations for a Cloudflare Durable Objects backend. That comparison informed the retired deployment, but it is no longer a runtime or deployment recommendation. Kratos now uses an application-managed Tailcat adapter for connectivity and a Rust headless engine as the authenticated durable sync/storage peer. No Durable Object, Worker, R2 bucket, WorkOS route, or provider SDK is required by production.

## Why the old decision no longer applies

The decisive architectural change is not the language used inside a Durable Object. Tailcat supplies a private TCP path, while the Kratos peer owns application authentication and the existing chat2/registry protocols. The peer persists update rows, checkpoints, sidecars, attachment custody, deduplication state, and backups in profile-scoped SQLite storage. This removes the hosted edge service rather than rewriting it in another language.

The active design preserves the useful parts of the former system:

- Loro document compatibility and the existing Rust session model;
- append-only chat rows, causal checkpoints/frontiers, resumable reads, and batch deduplication;
- conflict-resolved workspace registry rows and ephemeral presence;
- host-authoritative command execution and processed-before-execute journals;
- durable snapshots, sidecars and backups for new peer profiles, without importing old account data.

## Historical findings

For audit context only, the 2026 comparison found that TypeScript was the lower-risk implementation language *if Durable Objects remained the chosen platform*: Loro's npm package already wrapped its Rust/Wasm core; Workers WebSocket hibernation and R2 APIs were mature in TypeScript; and workers-rs had memory-lifecycle and JavaScript-boundary concerns for large CRDT documents. They must not be read as instructions to restore the old Worker.

The replacement starts fresh and provides no legacy-data import path. Repository cleanup does not delete live buckets, Durable Objects, backups, credentials, or local historical files. Infrastructure retirement remains a separate explicitly approved operation; old installed updater clients also need the bridge described in `docs/release-distribution.md`.
