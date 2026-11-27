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

`baremetal/` is OTW's own kernel: a UEFI machine boots it with no other operating system underneath. It brings up its own CPU tables, interrupts and page tables, all CPUs, the timer, PS/2 keyboard and mouse, NVMe and SATA (AHCI) disks, network cards — virtio, Intel's (e1000, e1000e including the I217/I218/I219 in Intel chipsets, igb/igc) and Realtek's (RTL8169/8168/8111/8125) — with TCP/IP (smoltcp, DHCP, DNS), and reads drive C: (a FAT32 partition named `OTW-C`, filled from `~/.otw/C` when the image is built). On top runs OTW's own library — the same shell and Win32 code as on Linux, built without `std` (`src/platform` stands in for it). Programs run natively in user mode, their threads on all CPUs; 64-bit Firefox browses the web on it.

```sh
make image      # build target/os/otw.img (40 MB, plus drive C:)
make os         # boot it in QEMU
```

What programs save, download and install on drive C: is written to the disk and stays there. Running `make image` again keeps it: the kernel is replaced, OTW's own files on C: are brought up to date and new files from `~/.otw/C` are added, but nothing already on C: is removed or overwritten (`OTW_FRESH=1 make image` starts from an empty drive instead). It refuses while a VM is running on the image. Drive C: has 2 GiB free (`OTW_DRIVE_C_FREE`, in MiB, when the image is made anew).

The image includes the App Center (`apps/app-center`: find and download programs from the winget repository) in `C:\Program Files\App Center`, with a shortcut on the desktop, when the Windows toolchain is installed: `scripts/build-app-center.sh --fetch` adds the Rust target `x86_64-pc-windows-gnullvm` and llvm-mingw (in `~/.local/share/otw-toolchain`, no root needed). Without it the image is built without the App Center (`OTW_APP_CENTER=0` leaves it out on purpose).

`make os` opens a window in a desktop session; without one the screen is at `vnc://127.0.0.1:5901` (`VM_DISPLAY=gtk` or `sdl` to choose). The kernel's log goes to the serial port (`VM_SERIAL=file` to keep it). `VM_NIC=e1000`, `e1000e` or `igb` gives the machine an Intel card instead of virtio. A kernel built with `OTW_PROFILE=1` logs where the CPUs' time goes every ten seconds (idle, program code by module, the subsystem by call), how threads were woken and how long pipe data and completion packets waited (`OTW_KERNEL_ENV=OTW_PROFILE_SECONDS=1` for a report every second).

On a real machine, write it to a USB stick and boot it with UEFI and Secure Boot off:

```sh
sudo dd if=target/os/otw.img of=/dev/sdX bs=4M conv=fsync status=progress
```

Each GitHub release has a ready image attached (`otw-<tag>.img.xz`, built by `.github/workflows/release.yml`): the kernel, OTW's test programs and free fonts on drive C:, no third-party programs. Unpack and write it the same way (`xz -dc otw-<tag>.img.xz | sudo dd of=/dev/sdX bs=4M conv=fsync status=progress`). It is a GPT disk image, not an ISO: the kernel reads FAT32 partitions on NVMe and SATA disks, not CDs.

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
