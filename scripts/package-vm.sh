#!/usr/bin/env bash
# Build a self-contained, statically linked otw-vm package for an external VM.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "$ROOT/Cargo.toml")"
OUT="$ROOT/dist/otw-vm-$VERSION"

rm -rf "$OUT"
mkdir -p "$OUT/bin" "$OUT/guest" "$OUT/guest/kernel" "$OUT/guest/boot" "$OUT/guest/disks" "$OUT/guest/net"

echo "==> building release binary (embedded target)"
"$ROOT/scripts/cargo.sh" build --offline --release --features embedded-target

echo "==> linking final binary as static-pie"
"$ROOT/scripts/cargo.sh" rustc --offline --release --features embedded-target \
  --bin otw-vm -- -C target-feature=+crt-static

install -m 0755 "$ROOT/target/release/otw-vm" "$OUT/bin/otw-vm"
install -m 0644 "$ROOT/binaries/win95/NOTEPAD.EXE" "$OUT/guest/NOTEPAD.EXE"
install -m 0644 "$ROOT/binaries/win95/PBRUSH.EXE" "$OUT/guest/PBRUSH.EXE"
install -m 0644 "$ROOT/binaries/win95/MSPAINT.EXE" "$OUT/guest/MSPAINT.EXE"
install -m 0644 "$ROOT/binaries/win95/MSPAINT.CNT" "$OUT/guest/MSPAINT.CNT"
install -m 0644 "$ROOT/binaries/win95/MSPAINT.HLP" "$OUT/guest/MSPAINT.HLP"
install -m 0644 "$ROOT/binaries/win95/MFC30.DLL" "$OUT/guest/MFC30.DLL"
install -m 0644 "$ROOT/binaries/win95/MFCO30.DLL" "$OUT/guest/MFCO30.DLL"
install -m 0644 "$ROOT/binaries/win95/MFCANS32.dll" "$OUT/guest/MFCANS32.dll"
install -m 0644 "$ROOT/binaries/disks/OTWDISK.IMG" "$OUT/guest/disks/OTWDISK.IMG"
install -m 0644 "$ROOT/binaries/guests/HELLO.EXE" "$OUT/guest/HELLO.EXE"
install -m 0644 "$ROOT/binaries/guests/WINSOCK.EXE" "$OUT/guest/WINSOCK.EXE"
install -m 0644 "$ROOT/binaries/kernels/HELLO.BIN" "$OUT/guest/kernel/HELLO.BIN"
install -m 0644 "$ROOT/binaries/boot/OTWBOOT.BIN" "$OUT/guest/boot/OTWBOOT.BIN"
install -m 0644 "$ROOT/binaries/boot/OTWVGA.BIN" "$OUT/guest/boot/OTWVGA.BIN"
cp -R "$ROOT/binaries/win95/dlls" "$OUT/guest/dlls"
cp -R "$ROOT/binaries/win95/net/." "$OUT/guest/net/"
install -m 0644 "$ROOT/README.md" "$OUT/README.md"
install -m 0644 "$ROOT/docs/win95-vm-architecture.md" "$OUT/win95-vm-architecture.md"
install -m 0644 "$ROOT/docs/packaging.md" "$OUT/VM-USAGE.md"

cat > "$OUT/run.sh" <<'EOF'
#!/bin/sh
set -eu
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$DIR/bin/otw-vm" "$@"
EOF
chmod 0755 "$OUT/run.sh"

(
  cd "$OUT"
  find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS
)

echo "==> package ready: $OUT"
echo "    binary:       $OUT/bin/otw-vm"
echo "    usage:        $OUT/VM-USAGE.md"
echo "    checksums:    $OUT/SHA256SUMS"
