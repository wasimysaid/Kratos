# Kratos

Control coding agents (Claude Code, Codex, Cursor, Devin, Grok, Hermes, Pi, and Mimir) locally by default, with optional private multi-device sync.

*English | [简体中文](README.zh-CN.md)*

![Kratos driving a Claude Code session with a live branch diff sidebar](apps/landing/public/assets/app-screenshot.jpg)

Every device runs a small engine and keeps a local cache. A new installation starts offline and local-only: no account, hosted backend, or provider credential is required.

## Install

Linux:

```bash
curl -fsSL https://github.com/wasimysaid/Kratos/releases/latest/download/install.sh | sh
kratos status
```

The installer verifies the GitHub Release manifest and checksum, installs the engine plus its managed Tailcat adapter, and starts a user service when systemd is available.

Mimir connects through `mimir acp`. Install the `mimir` CLI on the device that runs the agent; Kratos discovers it in Settings → Agents. The desktop sidebar browser also needs the [Linux browser runtime](docs/reference/linux-browser.md).

Useful commands:

```bash
kratos status
kratos update
kratos daemon start|stop|restart|status
```

On macOS, download the DMG from the [latest GitHub Release](https://github.com/wasimysaid/Kratos/releases/latest). On Windows, download the portable ZIP, keep `kratos-update.json` beside `kratos.exe`, and see the [source-build notes](docs/reference/windows-development.md).

## Optional multi-device sync

Tailcat provides private connectivity; Kratos's durable peer provides authenticated sync and storage. To support catch-up when laptops are not online together, initialize the peer on an always-on machine such as a VPS:

```bash
kratos daemon stop
kratos peer init --name home-peer
kratos daemon start
kratos peer invite --output invite.txt
```

Transfer `invite.txt` privately. On another stopped installation:

```bash
kratos daemon stop
kratos pair --code-file invite.txt
kratos daemon start
```

Invitations are short-lived and one-use. Pairing uses persistent device keys and proof of possession; list or revoke trusted devices with `kratos peer devices` and `kratos peer revoke <device-id>`. Keep pairing codes and Tailcat addresses private.

Packaged releases include the adapter. Source builds can run `scripts/build-tailcat.sh native` and set `KRATOS_TAILCAT_ADAPTER` to the resulting binary. Public DERP relays are rate-limited and have no SLA; operators needing durable relay service should initialize with an owned HTTPS DERP map using `--derp-map`.

Paired devices are trusted with the remotely permitted workspace surface. A controlling device can list, read, and write files on the owning device; enabling ignored-file visibility can expose files such as `.env`. `.git` remains excluded.

Sync starts fresh: creating a peer creates a new paired profile; joining an existing peer loads that peer's data. There is no "Bring my work" option, local-session import, or old cloud-account migration. Existing local and historical files are left untouched. Desktop setup switches runtimes in-app; headless users restart the engine. Return to local-only mode with:

```bash
kratos daemon stop
kratos logout
kratos daemon start
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the runtime and storage model and [docs/peer-backup.md](docs/peer-backup.md) for backup/restore of the new peer.

Licensed under the [MIT License](LICENSE).
