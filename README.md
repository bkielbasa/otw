# Open The Window (otw)

A reimplementation of Windows 95 in Rust that runs unmodified Windows programs on Linux. Programs run on an emulated x86/x64 CPU ([Unicorn](https://www.unicorn-engine.org/)); the kernel, the Win32 API and a Windows 95 style desktop are reimplemented.

## Requirements

Rust (stable), a C toolchain with CMake, Python 3 with `libclang`, and an X11 or Wayland display for the desktop. For the bootable image: the `x86_64-unknown-uefi` Rust target, and QEMU with OVMF to try it.

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

## Boot it on bare metal

`baremetal/` is OTW's own kernel: a UEFI machine boots it with no other operating system underneath. It brings up its own CPU tables, interrupts and page tables, all CPUs, the timer, PS/2 keyboard and mouse, NVMe and SATA (AHCI) disks, and reads drive C: (a FAT32 partition named `OTW-C`, filled from `~/.otw/C` when the image is built). Running programs on it is the work in progress.

```sh
make image      # build target/os/otw.img (40 MB, plus drive C:)
make os         # boot it in QEMU
```

On a real machine, write it to a USB stick and boot it with UEFI and Secure Boot off:

```sh
sudo dd if=target/os/otw.img of=/dev/sdX bs=4M conv=fsync status=progress
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
| `baremetal/` | The kernel for bare metal |

See [`docs/`](docs/) for details.

## License

GPL-2.0-only (see [`LICENSE`](LICENSE)). The UI font is rendered from DejaVu Sans (`binaries/fonts/LICENSE.DejaVu`). Not affiliated with Microsoft.
