#!/usr/bin/env bash
# Build the App Center (apps/app-center) as Windows programs for OTW:
# apps/app-center/target/x86_64-pc-windows-gnullvm/release/*.exe.
#
# It needs the Rust target x86_64-pc-windows-gnullvm and an llvm-mingw
# toolchain (the linker, and the C compiler the TLS library needs):
# OTW_LLVM_MINGW names its directory, else the newest
# ~/.local/share/otw-toolchain/llvm-mingw-* is used. With --fetch, both
# are installed first when missing (rustup, and llvm-mingw from
# github.com/mstorsjo/llvm-mingw, into ~/.local/share/otw-toolchain).
# Exits 2 when the toolchain is missing (make image goes on without it).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=x86_64-pc-windows-gnullvm
TOOLS="$HOME/.local/share/otw-toolchain"

find_mingw() {
  if [[ -n "${OTW_LLVM_MINGW:-}" ]]; then
    echo "$OTW_LLVM_MINGW"
  else
    ls -d "$TOOLS"/llvm-mingw-* 2>/dev/null | sort | tail -1
  fi
}

if [[ "${1:-}" == --fetch ]]; then
  rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"
  if [[ -z "$(find_mingw)" ]]; then
    url=$(curl -fsSL https://api.github.com/repos/mstorsjo/llvm-mingw/releases/latest |
      grep -o '"browser_download_url": "[^"]*ucrt-ubuntu-[^"]*x86_64\.tar\.xz"' | head -1 | cut -d'"' -f4)
    mkdir -p "$TOOLS"
    echo "==> fetching $url"
    curl -fsSL "$url" | tar -xJ -C "$TOOLS"
  fi
  shift
fi

MINGW="$(find_mingw)"
if ! rustup target list --installed | grep -qx "$TARGET" || [[ -z "$MINGW" || ! -x "$MINGW/bin/x86_64-w64-mingw32-clang" ]]; then
  echo "no Windows toolchain for the App Center (scripts/build-app-center.sh --fetch installs one)" >&2
  exit 2
fi

export PATH="$MINGW/bin:$PATH"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER=x86_64-w64-mingw32-clang
export CC_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-clang
export AR_x86_64_pc_windows_gnullvm=llvm-ar
cd "$ROOT/apps/app-center"
cargo build --release --target "$TARGET" "$@"
# Rust's unwinding (panics) is llvm-mingw's libunwind.dll: it goes with
# the programs.
cp "$MINGW/x86_64-w64-mingw32/bin/libunwind.dll" "$ROOT/apps/app-center/target/$TARGET/release/"
ls "$ROOT/apps/app-center/target/$TARGET/release/"*.exe
