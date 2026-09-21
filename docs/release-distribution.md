# Release distribution

Kratos releases are distributed only through GitHub Releases in
[`wasimysaid/Kratos`](https://github.com/wasimysaid/Kratos/releases). Cloudflare
R2 is not part of the steady-state release path.

## Release contract

A stable release tag is `v<major>.<minor>.<patch>` and must match the workspace
version. `.github/workflows/release.yml` attaches:

- `kratos-<version>-linux-x86_64.tar.gz`
- `kratos-<version>-linux-aarch64.tar.gz`
- `kratos-<version>-macos-arm64.dmg`
- `kratos-<version>-macos-arm64-app.tar.gz`
- `kratos-<version>-windows-x86_64.zip`
- `kratos-<version>-windows-x86_64.exe`
- `install.sh`
- `manifest.json`

`manifest.json` records the exact release version and a SHA-256 for every
attached file. Updaters and the installer fail closed when the selected asset
has no valid checksum. The default update feed is:

```text
https://github.com/wasimysaid/Kratos/releases/latest/download/manifest.json
```

GitHub resolves `latest` to the newest non-draft, non-prerelease release.
`KRATOS_RELEASES_URL` remains an explicit mirror/test override; normal clients
do not derive their release feed from the application edge URL.

## Publishing

1. Update the workspace version and release notes, then run the updater and
   installer tests.
2. Push `v<version>` only after all platform builds are expected to pass.
3. Confirm the GitHub Release has the eight assets listed above.
4. Download `manifest.json`, verify its `version`, and verify every listed
   SHA-256 against the corresponding release asset.
5. Check both stable redirects:

   ```bash
   curl -fIL https://github.com/wasimysaid/Kratos/releases/latest/download/manifest.json
   curl -fIL https://github.com/wasimysaid/Kratos/releases/latest/download/install.sh
   ```

Do not reintroduce R2 publication into the release workflow.

## One-time bridge from the legacy R2 feed

Clients older than the first GitHub-feed build still poll
`https://kratos.sh/releases`. Migrate them with exactly one bridge release; do
not keep dual publication as an ongoing fallback.

1. Keep the existing edge release route and R2 bucket readable while building
   the bridge version. Do not delete or rewrite old objects.
2. Publish the bridge tag through the GitHub workflow. Verify GitHub assets and
   checksums before changing the legacy feed.
3. From a separately authenticated operator checkout, download the six
   **versioned** bridge binaries and `manifest.json` from that GitHub Release.
   Verify the manifest locally. Upload those seven files once to the legacy R2
   bucket. Publish `manifest.json` only after all six binaries, then publish
   `latest.txt` containing the bridge version last. For example (replace
   `<version>` and use the production account configuration):

   ```bash
   tag="v<version>"
   mkdir -p /tmp/kratos-bridge && cd /tmp/kratos-bridge
   gh release download "$tag" --repo wasimysaid/Kratos \
     --pattern 'kratos-*' --pattern manifest.json
   jq -e --arg v "${tag#v}" '.version == $v' manifest.json
   jq -r '.files | to_entries[] | select(.key | startswith("kratos-")) | "\(.value.sha256)  \(.key)"' manifest.json \
     | sha256sum -c -
   for file in kratos-*; do
     npx wrangler@4 r2 object put "comet-native-releases/$file" \
       --file "$file" --remote
   done
   npx wrangler@4 r2 object put comet-native-releases/manifest.json \
     --file manifest.json --remote
   printf '%s' "${tag#v}" >latest.txt
   npx wrangler@4 r2 object put comet-native-releases/latest.txt \
     --file latest.txt --remote
   ```

4. Test an actual pre-bridge managed installation. It must discover the bridge
   on the legacy feed, verify/download it, restart into the bridge binary, and
   then resolve its next update check from GitHub without
   `KRATOS_RELEASES_URL`.
5. Leave the legacy bucket and route read-only for at least one full supported
   migration window (minimum 30 days, and longer if supported clients may be
   offline). Monitor for pre-bridge update requests. Do not publish later
   versions there.
6. After the window and verification, remove the legacy release route/Worker
   binding and its deployment credentials. Archive or delete the R2 bucket only
   through a separate, explicitly approved infrastructure operation. Repository
   cleanup must never delete live bucket contents.

A device that misses the bridge window can update by rerunning the GitHub
installer or manually installing a current GitHub Release. This is the bounded
recovery path and avoids permanently retaining the old service stack.
