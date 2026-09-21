# Hosted peer backup and disaster recovery

A hosted Tailcat peer is the durable sync authority. Backing up only
`peer/peer.sqlite` is **not** sufficient: that preserves rows and attachment
custody, but loses the trusted profile, device keys, pairing authority, and the
stable Tailcat address.

## Automatic generations

While a peer is hosted, the engine publishes a private generation every six
hours under:

```text
<data-dir>/peer/backups/<generation-uuid>/
```

Each generation contains:

- `peer.sqlite` — chat/registry logs, checkpoints, sidecars, nudges, and
  attachment custody;
- `auth.sqlite` — profiles, trusted/revoked devices, invites, challenges, and
  token records;
- `peer-device.json` — this engine's Ed25519 device identity;
- `peer-session.json` — profile/device binding, host role, loopback endpoint,
  DERP configuration, and Tailcat address;
- `tailcat-server.key` — Tailcat server private key, preshared key, and selected
  relay region;
- `manifest.json` — exact inventory, sizes, SHA-256 digests, profile/device/
  address binding, creation time, and the authorization rollback warning.

SQLite files are made with SQLite snapshot operations; live database, `-wal`,
and `-shm` files are never copied. Stable secret files are read before and
after snapshotting, and a generation is rejected if they changed. On Unix, files
are `0600` and directories are `0700`; on Windows, protect the data and export
locations with account-restricted ACLs. The manifest is written last, and the
complete directory is atomically renamed into place. `latest.json` is only an
atomic pointer; a generation remains independently verifiable.


Regular files are flushed before publication. Unix also uses staging-directory
and parent-directory `fsync`. Windows flushes writable file handles and publishes
same-parent moves with `MoveFileExW(MOVEFILE_WRITE_THROUGH)`; the latest pointer
uses atomic replacement. Windows does not provide the POSIX directory-fsync
operation, so no directory-handle flush is attempted. File-flush and publication
errors are reported. These checks do not replace power-loss testing of the actual
filesystem and storage hardware.

To create and copy a fresh generation to operator-controlled storage, first
stop Kratos on the hosted peer, then run:

```sh
KRATOS_DATA_DIR=/srv/kratos kratos peer backup \
  --output-dir /mnt/encrypted/kratos-peer-backups
```

The command takes the normal exclusive engine lock and fails if an engine is
still using that data directory. The output directory is created privately when
needed, and the command prints the complete
`<output-dir>/<generation-uuid>` path. Copy or retain that complete generation;
it contains private keys and bearer authorization state and must be encrypted
and access-controlled like the live server.

## Offline restore drill

Stop every Kratos process using the source or destination. Select one complete
generation and restore it to a new data directory:

```sh
kratos peer restore \
  --generation /mnt/encrypted/kratos-peer-backups/<generation-uuid> \
  --destination /srv/kratos-restored \
  --acknowledge-trust-rollback
```

The destination must be absent; even an empty existing directory is rejected.
Restore never merges with an existing installation. The command verifies the
manifest version and exact inventory, denies symlinks/path traversal, checks
every length and digest, runs SQLite `quick_check`, and validates the session's
profile/device/address binding. It restores into a private staging tree and
checks the ordinary engine lock there. Unix holds the lock across publication;
Windows closes staging handles before its atomic no-overwrite rename. There are
no writes after publication, so an engine starting at the destination receives
the complete restored tree and acquires its usual instance lock. Start the peer
normally with
`KRATOS_DATA_DIR=/srv/kratos-restored kratos headless`; its profile, trusted device
identity, and Tailcat address are unchanged.

### Authorization rollback policy

A backup is a point-in-time copy. Restoring it also rolls trusted-device and
revocation state back to that time. The CLI refuses to proceed unless
`--acknowledge-trust-rollback` is supplied. Before exposing the restored peer,
revoke or rotate any device compromised after the generation was created. If
the host authority or backup itself may be compromised, do not reuse it: create
a new profile and pair trusted devices again.

## Peer backup versus whole-host backup

The peer generation replaces the former Durable Object/R2 durability role: it
covers server-side sync logs, checkpoints, sidecars, attachment custody, auth,
and Tailcat identity. It does **not** claim to be a whole-machine backup.

A machine that also executes agents has profile-local material outside the peer
service, including `<data-dir>/profiles/<profile-uuid>/` snapshots and journals,
upload staging/cache, repositories/worktrees, terminal state, agent credentials,
and settings. Back those up with the host's normal encrypted filesystem backup.
Restore the peer generation only while Kratos is stopped, then restore the host
files using the same point-in-time policy; never copy live SQLite WAL files.
