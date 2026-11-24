#!/usr/bin/env bash
# Run cargo with the x86 backend's build-time toolchain configured.
#
# Unicorn's bindings need libclang, and its CMake build prefers ninja. When a
# Python environment with the `libclang`, `cmake`, and `ninja` packages is
# available, use it automatically. Callers can still override PATH,
# LIBCLANG_PATH, and BINDGEN_EXTRA_CLANG_ARGS themselves.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# A previous build may have configured Unicorn's CMake with tools from a
# temporary Python environment. If that environment disappears, the cached
# CMAKE_MAKE_PROGRAM/CMAKE_COMMAND paths remain and CMake fails before it can
# regenerate them. Drop those stale build directories so cargo can reconfigure.
for cache in "$ROOT"/target/*/build/unicorn-engine-sys-*/out/build/CMakeCache.txt; do
  [[ -f "$cache" ]] || continue
  stale=0
  while IFS= read -r line; do
    case "$line" in
      CMAKE_MAKE_PROGRAM:FILEPATH=* | CMAKE_COMMAND:INTERNAL=*)
        tool="${line#*=}"
        [[ -x "$tool" ]] || stale=1
        ;;
    esac
  done < "$cache"
  if [[ "$stale" -eq 1 ]]; then
    rm -rf "$(dirname "$(dirname "$(dirname "$cache")")")"
  fi
done

python=""
for candidate in "${OTW_PYTHON:-}" python3 /tmp/opencode/pyenv/bin/python; do
  if [[ -n "$candidate" ]] && command -v "$candidate" >/dev/null 2>&1 \
      && "$candidate" -c 'import clang.cindex' >/dev/null 2>&1; then
    python="$candidate"
    break
  fi
done

export BINDGEN_EXTRA_CLANG_ARGS="${BINDGEN_EXTRA_CLANG_ARGS:--I$ROOT/.cargo/clang-include}"

if [[ -n "$python" ]]; then
  python_bin="$(cd "$(dirname "$python")" && pwd)"
  case ":$PATH:" in
    *":$python_bin:"*) ;;
    *) export PATH="$python_bin:$PATH" ;;
  esac

  if [[ -z "${LIBCLANG_PATH:-}" ]]; then
    LIBCLANG_PATH="$("$python" -c 'import pathlib, clang.cindex; print(pathlib.Path(clang.cindex.__file__).parent / "native")')"
    export LIBCLANG_PATH
  fi
fi

cd "$ROOT"
exec cargo "$@"
