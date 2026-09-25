# GPUI parity inventory

## Goal

This app reproduces the GPUI application's product surface on Android, iOS,
and web. Phone and narrow-screen layout may adapt, but a feature is not removed
because it does not fit the current iOS client.

Baseline: `feat/web-ui-kratos` at `eeee0715`.

## Workstreams

| Surface | GPUI reference | Mobile requirement |
| --- | --- | --- |
| Shell and navigation | `crates/ui/src/shell.rs`, `shell/` | Spaces, sessions, tabs, archive, history, rails, routes, and responsive panels. |
| Transcript | `crates/ui/src/transcript.rs`, `markdown/` | Streaming rows, markdown, code, tool details, artifacts, follow behavior, selection, and links. |
| Composer and queue | `crates/ui/src/composer.rs`, `queue.rs`, `attachments.rs` | Drafts, mentions, slash commands, attachments, questions, run/steer/interrupt, and queue editing. |
| Workspace | `crates/ui/src/files/` | Tree, search, read/write editor, previews, media, tabs, and workspace watches. |
| Terminal | `crates/ui/src/terminal/` | Open, input, resize, streamed output, and lifecycle controls. |
| Changes and reviews | `crates/ui/src/changes.rs`, `comment_ui.rs`, `comments.rs` | Checkout diffs, file navigation, inline comments, and change-request views. |
| Browser and previews | `crates/ui/src/browser/` | Browser surface, preview lifecycle, and browser-specific controls. |
| Settings and account/device flows | `crates/ui/src/settings/`, `pairing.rs`, `app_menus.rs` | Appearance, agents, devices, accounts, notifications, shortcuts where applicable, and pairing. |
| Shared visual system | `crates/ui/src/theme.rs`, `typography.rs`, `motion.rs`, `icons.rs` | Tokens, fonts, icons, status language, and motion faithful to GPUI. |

## Platform contracts

The UI is shared TypeScript. Transport is intentionally platform-specific:

| Platform | Required adapter |
| --- | --- |
| Android | Expo module wrapping `target/tailcat/KratosTailcat.aar`, secure device identity, peer authentication, and local cache. |
| iOS | Expo module wrapping the existing Tailcat XCFramework path, secure device identity, peer authentication, and local cache. |
| Web | Existing browser-gateway authentication and WebSocket transport; no native Tailcat bridge. |

The client must support the complete `ControlRpc` surface, including streamed
terminal, filesystem, preview, and checkout-diff operations. The existing iOS
client is not sufficient for that contract.

## First gate

Before implementing feature UI, prove all of the following on an Android
device:

1. Tailcat starts through an Expo development build and exposes its loopback endpoint.
2. Pairing proofs use a persistent device key and match the peer contract.
3. Registry and chat documents survive restart, reconnect, and offline reads.
4. Device-relay RPC can make a unary call and receive a stream.

No GPUI feature is intentionally deferred; the workstreams establish delivery
order rather than a reduced scope.
