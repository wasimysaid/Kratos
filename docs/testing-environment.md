# Linux native UI verification

This checkout can verify the Linux browser helper against the distribution's real
WebKitGTK and JSON-GLib libraries without installing packages system-wide. The
commands below were run on Ubuntu 26.04.1 (`x86_64`, package versions from
`resolute`/`resolute-updates`). Downloads are performed with `apt-get download`
and unpacked with `dpkg-deb -x` into `target/native-sysroot`; `sudo` is not
needed.

## Local native environment

```sh
set -eu
REPO="$PWD"
SYSROOT="$REPO/target/native-sysroot"
TOOLS="$REPO/target/native-tools"
mkdir -p "$SYSROOT/debs" "$SYSROOT/rootfs" "$TOOLS"

# Development metadata and headers for the native helper and its xcb/xkbcommon
# link dependencies. Include the roots as well as their transitive dependencies.
roots='libwebkit2gtk-4.1-dev libjson-glib-dev libgtk-3-dev libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libxcb-xkb-dev'
{
  printf '%s\n' $roots
  apt-cache depends --recurse --important $roots |
    awk '/^  (PreDepends|Depends):/ {
      sub(/^  (PreDepends|Depends):[[:space:]]*/, "")
      gsub(/[<>|]/, "", $1)
      if ($1 ~ /^[A-Za-z0-9][A-Za-z0-9+.-]*$/) print $1
    }'
} | sort -u > "$SYSROOT/packages.txt"
(cd "$SYSROOT/debs" && apt-get download $(cat ../packages.txt))

# Real local test/display tools. x11-apps supplies xwd; libxdo3 is xdotool's
# runtime dependency. These archives are also extracted, never installed.
(cd "$SYSROOT/debs" && apt-get download xvfb xdotool libxdo3 imagemagick imagemagick-7.q16 x11-apps)
for deb in "$SYSROOT"/debs/*.deb; do
  dpkg-deb -x "$deb" "$SYSROOT/rootfs"
done

ln -sfn /usr/bin/gcc "$TOOLS/gcc"
ln -sfn /usr/bin/cc "$TOOLS/cc"
ln -sfn /usr/bin/pkg-config "$TOOLS/pkg-config"
for tool in Xvfb xdotool xwd magick-im7.q16; do
  ln -sfn "$SYSROOT/rootfs/usr/bin/$tool" "$TOOLS/$tool"
done
# The fixture uses ImageMagick 6's `import -window ID FILE` spelling. This
# project-local adapter captures that window with the extracted xwd + Magick 7.
cat > "$TOOLS/import" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} != -window || $# != 3 ]]; then
  echo "usage: import -window WINDOW_ID OUTPUT.png" >&2
  exit 2
fi
tmp="$(mktemp --suffix=.xwd)"
trap 'rm -f "$tmp"' EXIT
xwd -silent -id "$2" -out "$tmp"
magick-im7.q16 "$tmp" "$3"
EOF
chmod +x "$TOOLS/import"
```

Use only these explicit paths for the native checks. In particular, do not add
`target/mimir-tools` to `PATH`; its `pkg-config` and compiler files are test
wrappers, not native toolchain components.

```sh
export PKG_CONFIG_SYSROOT_DIR="$SYSROOT/rootfs"
export PKG_CONFIG_LIBDIR="$SYSROOT/rootfs/usr/lib/x86_64-linux-gnu/pkgconfig:$SYSROOT/rootfs/usr/share/pkgconfig"
export PKG_CONFIG_PATH="$PKG_CONFIG_LIBDIR"
export PATH="$TOOLS:/usr/bin:/bin:$HOME/.cargo/bin"
export CC="$TOOLS/gcc"
export LIBRARY_PATH="$SYSROOT/rootfs/usr/lib/x86_64-linux-gnu:$SYSROOT/rootfs/lib/x86_64-linux-gnu"
export LD_LIBRARY_PATH="$SYSROOT/rootfs/usr/lib/x86_64-linux-gnu:$SYSROOT/rootfs/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu:/lib/x86_64-linux-gnu"
```

## Native checks

Real package discovery and compiler/link verification:

```sh
pkg-config --version
pkg-config --modversion webkit2gtk-4.1 json-glib-1.0 xkbcommon xkbcommon-x11 xcb xcb-xkb
flags=( $(pkg-config --cflags --libs webkit2gtk-4.1 json-glib-1.0) )
"$CC" -std=c11 -O2 -Wall -Wextra -Wno-unused-parameter \
  crates/ui/src/browser/linux/helper.c \
  -o "$TOOLS/kratos-webkit-real" "${flags[@]}"
ldd "$TOOLS/kratos-webkit-real"
```

Expected versions for the Ubuntu 26.04.1 archives used here are WebKitGTK
`2.52.6`, JSON-GLib `1.10.8`, xkbcommon `1.13.1`, and xcb `1.17.0`. `ldd`
should resolve WebKitGTK, JSON-GLib, GTK, GLib, xcb, and xkbcommon from
`target/native-sysroot/rootfs/usr/lib/x86_64-linux-gnu`.

A direct native display/helper smoke check (the helper exits on stdin EOF, so
stdin must remain open) is:

```sh
runtime="$(mktemp -d)"
trap 'rm -rf "$runtime"' EXIT
"$TOOLS/Xvfb" -displayfd 3 -screen 0 1280x800x24 -ac -nolisten tcp \
  3>"$runtime/display" >"$runtime/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in $(seq 1 100); do test -s "$runtime/display" && break; sleep .1; done
export DISPLAY=":$(cat "$runtime/display")"
"$TOOLS/xdotool" getdisplaygeometry
(sleep 5) | "$TOOLS/kratos-webkit-real" >/dev/null 2>"$runtime/helper.log" &
helper_pid=$!
sleep 2
kill -0 "$helper_pid" # native WebKit helper stayed alive on Xvfb
wait "$helper_pid" || true
kill "$xvfb_pid"; wait "$xvfb_pid" || true
```

For the full real browser fixture, build and run the repository script with the
same exports above:

```sh
cargo build --release --locked -p kratos-ui \
  --example browser-fixture --features browser-fixture
BROWSER_FIXTURE_BINARY=target/release/examples/browser-fixture \
  bash scripts/test-linux-browser.sh target/verification/linux-browser
```

The script uses a local Xvfb display and captures only its fixture window. It
also exercises the Linux native-pointer path, navigation/history, shared
ephemeral browser data, and native screenshots. `target/verification` is local
verification output and is not a user-desktop capture.

## Verification record

The following checks were run in this checkout:

* `pkg-config` was the real `/usr/bin/pkg-config` through the project-local
  symlink and returned WebKitGTK `2.52.6`, JSON-GLib `1.10.8`, xkbcommon
  `1.13.1`, and xcb `1.17.0`.
* The C helper compiled and linked successfully with `CC`, `LIBRARY_PATH`, and
  the sysroot-aware pkg-config flags; `ldd` resolved the required native
  libraries from the extracted sysroot.
* The extracted Xvfb and xdotool ran together on a fresh display, and the
  extracted WebKit helper remained alive for two seconds under that display.
* An isolated Xvfb `xclock` window was captured to
  `target/verification/native-xvfb/xclock.png` with the project-local adapter;
  the PNG was inspected and measured as `502x182`.
* The explicit main build `cargo build --release --locked -p kratos --bin kratos`
  passes, and the combined main/fixture release build also passes.
* `kratos --help`, `peer --help`, `pair --help`, `sync --help`,
  and `daemon --help` all exit
  successfully. A fresh `KRATOS_DATA_DIR` `kratos status` reports signed-out,
  local-only state without secrets.
* The actual main binary launched under isolated Xvfb with
  `KRATOS_NO_LOGIN_SHELL=1`, assembled a fresh local engine/device, and exposed
  a window. The real app Devices management view was captured and inspected
  at `target/verification/kratos-native-attempt5/settings-management2.png`
  (1280x800): the Devices page shows the local device, private sync-peer
  status, local-only scope, and Rename/Refresh controls with no clipping.
* The browser fixture also ran to completion with exit code 0 under real
  WebKitGTK/Xvfb and generated fresh native render/context/IME/resize/select
  screenshots under `target/verification/browser-native-attempt4/`; representative
  context/menu states were inspected.
* The fixture logs retain non-fatal XInput-pointer, software-EGL, and
  clipboard-manager warnings; none prevented rendering or capture.
* The actual local-only app was launched under the same isolated Xvfb display;
  its account menu exposed `Local only` and `Enable sync` without requiring an
  invitation or browser authentication.
* `Enable sync` opened the real pairing gate. Fresh captures were inspected at
  `target/verification/pairing-native-final2/pairing-choice.png`,
  `target/verification/pairing-native-final4/create-form.png`,
  `target/verification/pairing-native-final4/join-form.png`, and
  `target/verification/pairing-native-final4/back-choice.png`. The create form
  showed the default `This device` name, while the join form showed the
  one-time `kratos-pair` invitation field without exposing any secret.
* Keyboard focus was exercised on the pairing cards with Tab + Return; the
  activation opened the create form, and the pointer-driven run opened the join
  form and returned with Back. These controls are also covered by the existing
  pairing accessibility tests.
