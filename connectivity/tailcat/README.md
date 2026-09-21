# Kratos Tailcat native adapter

This module is the production connectivity shim between Kratos's Rust HTTP/WebSocket peer and [Tailcat](https://github.com/tailscale/tailcat). It pins upstream commit `fd101889796a947ac514e9d86ec731af2965fad3` as Go pseudo-version `v0.6.1-0.20260913000754-fd101889796a`.

## Managed CLI contract

The parent process starts and owns one subprocess per role. Standard output contains exactly one readiness JSON line; diagnostics go to standard error and never include the secret Tailcat address.

The Rust launcher sets `KRATOS_TAILCAT_PARENT_PIPE=1` and retains the write end of
an otherwise unused stdin pipe. EOF terminates the adapter even when native app
termination or a crash bypasses Rust destructors. Standalone CLI usage without
this environment variable remains signal-controlled. Normal engine shutdown also
explicitly stops and waits for its adapter without deleting the saved pairing.

```text
kratos-tailcat serve \
  --state /private/profile/server.key \
  --target 127.0.0.1:<rust-peer-port> \
  [--derp-map https://example/derpmap.json] [--region <id>]

=> {"address":"tc..."}
```

`serve` starts a persistent Tailcat server, listens only on Tailcat TCP application port `7332`, and proxies each connection only to the exact numeric IPv4 loopback target. It is not an exit node or unrestricted forwarder.

```text
kratos-tailcat connect \
  --config /private/profile/connect.json \
  --listen 127.0.0.1:0 \
  --state /private/profile/client.key \
  [--derp-map https://example/derpmap.json]

=> {"url":"http://127.0.0.1:<port>"}
```

`connect.json` has the exact schema `{"address":"tc..."}` and must be a regular mode-`0600` file. The secret is never accepted in argv. `connect` rejects unknown fields and trailing JSON, verifies connectivity before readiness, binds only numeric `127.0.0.1`, and sends every accepted stream to Tailcat application port `7332`. HTTP, streaming bodies, and WebSockets pass through as ordinary TCP bytes. SIGINT or SIGTERM closes listeners, active streams, and the Tailcat engine.

Tunnel TCP dials have a five-second attempt budget. After failure, the client
recreates its Tailcat network stack using the same persisted identity, coalescing
concurrent failures and rate-limiting resets. This repeats the registration that
the pinned upstream client otherwise caches across a server restart. The local
listener stays stable. Only connection establishment is retried; application
bytes are never replayed by the adapter. Room supervisors still own stream
reconnection and durable application-level catch-up.

State is role-tagged so client/server identities cannot be interchanged. Secret files are atomically written with mode `0600` under a `0700` directory. Existing symlinks, non-regular files, incorrect permissions, malformed JSON, missing keys, and role mismatches fail closed rather than generating a replacement identity.

## Native binding contract

The root Go package is directly gomobile-bindable:

```go
client, err := tailcatnative.StartClient(address, stateDir, derpMap)
client.URL()
client.Close()

server, err := tailcatnative.StartServer(target, stateDir, derpMap)
server.Address()
server.Close()
```

The binding creates `client.key` or `server.key` in `stateDir`. It uses userspace networking only: no command execution, process spawning, privileged VPN, route changes, DNS changes, or arbitrary destination forwarding.


Release packages place `kratos-tailcat` adjacent to `kratos` (inside `Contents/MacOS` on macOS). The Windows ZIP is the supported portable download and contains both executables plus a checksum-bound preserve policy; the versioned standalone `kratos-...exe` release asset is updater payload for an existing ZIP installation, not a complete fresh installation. Windows uses the containing profile directory's private ACL because POSIX `0600` mode bits are not represented there.

## Build

From the repository root:


Apple binding builds require the pinned `gomobile` and `gobind` commands. The
`gobind` tool directive in `go.mod` keeps the matching mobile module in the
binding graph across `go mod tidy`:

```sh
go install golang.org/x/mobile/cmd/gomobile@v0.0.0-20260908204917-8b95e45f8d3e
go install golang.org/x/mobile/cmd/gobind@v0.0.0-20260908204917-8b95e45f8d3e
```

```sh
scripts/build-tailcat.sh native     # host CLI
scripts/build-tailcat.sh cross      # desktop/headless release matrix
scripts/build-tailcat.sh xcframework # macOS host; iOS device + simulator
scripts/build-tailcat.sh all
```

Artifacts are written under `target/tailcat/` by default. The iOS output is exactly `target/tailcat/KratosTailcat.xcframework`. Set `OUT_DIR` or `GO` to override those locations.
