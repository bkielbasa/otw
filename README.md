# Open The Window (otw)

A reimplementation of Windows 95 in Rust that runs unmodified Windows programs on Linux. Programs run on an emulated x86/x64 CPU ([Unicorn](https://www.unicorn-engine.org/)); the kernel, the Win32 API and a Windows 95 style desktop are reimplemented.

## Requirements

Rust (stable), a C toolchain with CMake, Python 3 with `libclang`, and an X11 or Wayland display for the desktop.

## Usage

```sh
make test       # build and run the tests
make desktop    # start the desktop
```

The desktop's drive C: is `~/.otw/C` (set `OTW_DRIVE_C` to change it). Put programs there and start them from the Start menu or the desktop.

Run a single program:

```sh
make run TARGET=path/to/PROGRAM.EXE FEATURES=display ARGS="--run --window"
```

Microsoft's files are not included. Tests that need them (copied into `binaries/win95/`) are skipped when they are missing.

## Layout

| | |
|---|---|
| `src/kernel/` | PE loading, CPU, processes, scheduling, exceptions |
| `src/win32/` | The Win32 API DLLs |
| `src/vm.rs`, `src/session.rs` | Programs and the desktop session |
| `src/shell/` | The desktop shell |
| `src/host/` | The host window |

See [`docs/`](docs/) for details.

## License

GPL-2.0-only (see [`LICENSE`](LICENSE)). The UI font is rendered from DejaVu Sans (`binaries/fonts/LICENSE.DejaVu`). Not affiliated with Microsoft.
