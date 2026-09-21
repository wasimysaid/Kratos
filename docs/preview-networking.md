# Project previews

Run an HTTP development server in a project and open a Browser tab in that
session. The empty tab lists its running services. **Open** navigates to
`http://<device>.<project>.localhost:7331`; additional services receive persistent
names such as `<device>.<project>-api.localhost:7331`.

The daemon owns this listener, discovery and peer connections. Closing a browser
tab does not stop discovery or change an address. Account teardown cancels the
proxy, discovery, signaling, and existing peer streams before the next profile
starts. A busy proxy port produces an inline error instead of changing the URL.

## Discovery and identities

On Linux, the scanner joins the current user's `/proc` process cwd, ancestry,
creation time and socket descriptors to listening TCP sockets. On macOS it uses
`lsof` and `ps` for equivalent metadata. Only loopback-reachable listeners whose
cwd belongs to a known local project are probed. The deepest matching project
wins. Unrelated listeners and non-HTTP services are excluded. HTTP probes are
bounded and run every two seconds, using HEAD and accepting valid HTTP status
responses (including authentication and application errors).

Kratos terminal/task/agent descendants are marked as Kratos-owned. Framework
commands identify Vite, Next.js, Astro, Miniflare and Node servers; otherwise the
list uses a generic HTTP label. Before a local backend connection, the daemon
rechecks the listener's process identity and cwd to reject stale port reuse.

The profile's `previews.json` stores project and service IDs and hostname labels
separately from observed PIDs and ports. Command identities omit common port
options. Device-name collisions get persisted suffixes. The first service keeps
the project hostname; additional services get descriptive suffixes. Two
indistinguishable instances of the same command in the same cwd get separate
slots; their individual identities cannot be inferred across simultaneous
restarts without additional application-provided identity.

`WatchPreviews` resolves the session's current cwd on each catalog or workspace
change. On the viewing device it selects either the local services or the
advertised services for the session's owning device. Changing the active
checkout therefore changes the list without copying paths or entering ports.

## HTTP and stream transport

On macOS 14 and later, WebKit receives a per-domain HTTP CONNECT configuration
for preview hostnames, avoiding older macOS DNS behavior without system changes.
The CONNECT endpoint admits only known preview names at the proxy port, and its
tunnels share bounded limits and profile cancellation. Other website traffic
retains normal routing.

The loopback proxy validates the Host against its catalog, then opens an opaque
service ID through a transport-independent multiplexer. Locally, framed streams
travel over a socket pair. Remotely, the same bounded frames travel through the
authenticated preview WebSocket on the durable peer; the peer stamps the source
and routes only to a paired target device. The hosting engine resolves only its
own current service IDs, so remote metadata never authorizes an arbitrary TCP
address.

Frames have a one-byte kind, a big-endian 32-bit stream ID, and a bounded payload.
Kinds are OPEN, DATA, END, CANCEL, WS_OPEN, WS_DATA, WS_CLOSE, READY and CREDIT.
Each device pair gets an independent multiplexer and deterministic stream-ID
parity. DATA payloads are at most 8 KiB; each stream has a 64 KiB receive window;
connection queues are bounded; and up to 64 streams share a connection. END
half-closes, CANCEL tears down both directions, graceful shutdown drains buffered
bytes, and dropping an unfinished request cancels promptly.

Hyper streams request and response bodies. The proxy removes hop-by-hop headers,
preserves Host and Origin together (including Next.js Server Actions), sets
X-Forwarded-Host, and rewrites absolute localhost redirects. Set-Cookie and other
end-to-end headers remain intact. WebSocket upgrades retain their byte stream,
subprotocol and close handshake, allowing Vite HMR through the same URL.

## Presence and authenticated relay

Each paired engine authenticates to the durable peer with its profile-bound
device identity. The peer accepts bounded service catalogs, stamps the source of
opaque preview frames, and prevents clients from choosing another profile or
forging a sender. Catalog presence has a heartbeat lease; disconnect and
revocation remove advertised routes and active peer multiplexers.

Tailcat protects the private HTTP/WebSocket path to the peer. Kratos's application
authorization, target assignment, frame bounds, flow control, and service-ID
containment remain mandatory above that transport. A relay connection is opened
lazily when a remote preview is requested. Local previews remain independent of
peer availability. macOS and Linux currently provide process discovery.

## Validation

`cargo test --locked -p kratos-preview` covers real process/cwd isolation, non-HTTP
exclusion, live disappearance, persistent aliases, port changes, concurrent
streams, slow readers, cancellation, large bodies, streaming HTTP headers,
redirects, WebSocket traffic, bounded peer envelopes, and authenticated catalog
routing through a local Rust peer fixture.

Build `cargo build -p kratos-ui --example preview-fixture --features browser-fixture`.
Run the fixture with an output directory, an available display and `VITE_BINARY`
pointing to an installed `vite/bin/vite.js`. It starts real Vite/API processes in
an isolated project, discovers them through daemon RPC and waits for a native
click on Vite's Open button. It then verifies HMR, disappearance and a port-change
restart while capturing the native UI. Screenshots/videos belong in PR user
attachments, not the repository.
