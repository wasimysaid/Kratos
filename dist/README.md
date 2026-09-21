# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/kratos-<version>-linux-<arch>.tar.gz` containing:

- `kratos` — the binary (headed by default; `kratos headless` runs the engine alone)
- `kratos.desktop` — XDG desktop entry
- `kratos.png` — 1024×1024 Kratos app icon
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/kratos-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Kratos.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
dmg. The auto-update tarball retains an internal `Kratos.app` path so older
installed builds can update into Kratos. CI runs this on tags
(`.github/workflows/release.yml`). The manual steps it automates, for reference
(run on a macOS host — gpui needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p kratos --target aarch64-apple-darwin
   cargo build --release -p kratos --target x86_64-apple-darwin
   lipo -create -output kratos \
     target/aarch64-apple-darwin/release/kratos \
     target/x86_64-apple-darwin/release/kratos
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Kratos.app/Contents/{MacOS,Resources}
   cp kratos Kratos.app/Contents/MacOS/kratos
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Kratos.app/Contents/Info.plist
   ```
3. Icon: generate `kratos.icns` from `dist/macos/icon-1024.png` (the macOS-shaped
   variant of the artwork — squircle mask, margins, and shadow pre-baked, since
   `sips` can't apply an alpha mask) and place it at
   `Kratos.app/Contents/Resources/kratos.icns`:
   ```sh
   mkdir kratos.iconset && sips -z 256 256 dist/macos/icon-1024.png --out kratos.iconset/icon_256x256.png
   iconutil -c icns kratos.iconset -o Kratos.app/Contents/Resources/kratos.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Kratos.app
   xcrun notarytool submit Kratos.zip --keychain-profile … --wait
   xcrun stapler staple Kratos.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Kratos -srcfolder Kratos.app -ov -format UDZO Kratos.dmg`).
