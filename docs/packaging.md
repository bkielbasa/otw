# Packaging otw-vm for an external VM

This package is the **Rust VM host binary**, not a Windows guest executable. It is built as a statically linked x86-64 Linux PIE so it can be copied into almost any Linux virtual machine and run without installing Rust, libclang, or shared libraries.

## Build

From the workspace root:

```sh
./scripts/package-vm.sh
```

The script builds with `scripts/cargo.sh`, which configures the x86 backend's build-time tools (`cmake`, `ninja`, and `libclang`) and uses the checked-in clang header shims. The final binary is then linked as static-pie with `cargo rustc`.

The script produces:

```text
dist/otw-vm-0.1.0/
├── bin/otw-vm                  # static-pie, stripped, self-contained
├── guest/NOTEPAD.EXE           # genuine Win95 RTM target (also embedded in the binary)
├── guest/PBRUSH.EXE            # genuine RTM Paint launcher (compatibility test)
├── guest/MSPAINT.EXE           # genuine RTM Paint payload (traced, not fully runnable)
├── guest/MSPAINT.CNT           # Paint help contents
├── guest/MSPAINT.HLP           # Paint help file
├── guest/MFC30.DLL             # genuine MFC used by MSPAINT
├── guest/MFCO30.DLL            # genuine MFC OLE support
├── guest/MFCANS32.dll          # genuine MFC ANSI support
├── guest/HELLO.EXE             # hand-built hello guest (also embedded in the binary)
├── guest/WINSOCK.EXE           # hand-built Winsock IAT/TCP round-trip guest
├── guest/kernel/HELLO.BIN      # freestanding x86 kernel image (also embedded)
├── guest/boot/OTWBOOT.BIN      # 512-byte real-mode text boot sector (also embedded)
├── guest/boot/OTWVGA.BIN       # 512-byte VGA mode-13h boot sector (also embedded)
├── guest/disks/OTWDISK.IMG     # deterministic FAT32 Explorer demo image
├── guest/dlls/                 # 7 extracted Win95 system DLLs for future API work
├── guest/net/                  # genuine Winsock artifacts + SHA256SUMS (reference only)
├── run.sh                      # convenience launcher
├── README.md
├── win95-vm-architecture.md
├── VM-USAGE.md                 # this document
└── SHA256SUMS
```

The `embedded-target` Cargo feature embeds Notepad, the hello guest, the kernel image, and both boot sectors with `include_bytes!`, so those paths run with no external files. Paint artifacts, the generated Winsock guest, genuine Winsock reference files, and the FAT32 image are copied for inspection and host-side tests, but are not embedded. The optional `display` feature (winit/glutin/glow host window) is deliberately not enabled in this package.

## Binary properties

```text
ELF 64-bit LSB pie executable, x86-64, static-pie linked, stripped
```

- No Rust toolchain, libclang, glibc, or crate runtime needed inside the VM.
- Host OS: any x86-64 Linux with kernel 3.2+ (static-pie requirement).
- Current behavior: parses an embedded PE, maps sections into flat guest memory, installs synthetic IAT thunks with handlers for all 127 Notepad imports, and executes guest code on the x86 backend. It also supports multi-module mapping of genuine companion DLLs and the Rust WinSock 1.1 layer backed by host networking. The genuine Notepad target can be executed headlessly, but it then waits in its real message loop; the interactive virtual desktop with generic program icons, taskbar process list, draggable/resizable/focusable multiwindow support, icon launching, and the Explorer shell window requires a source build with `--features display`, which is excluded from this static-pie package. Headless FAT32 listing is included in the static binary.

## Run the hello guest

Copy the package directory into the VM, then:

```sh
cd otw-vm-0.1.0
./run.sh --hello --run
```

Expected output:

```text
target: embedded:binaries/guests/HELLO.EXE
PE32 machine=0x014c base=0x00400000 entry=0x00401000 sections=1
imports: 3 functions from 1 DLLs
  KERNEL32.dll   3
mapped 1048588 bytes; 3 IAT thunks installed; 3 imports have Rust handlers
Hello from the OTW Win95 VM!
guest exited with status 0x00000000 after 3 API calls
```

## Run the Winsock IAT guest

`guest/WINSOCK.EXE` is a hand-built PE that exercises the Rust WinSock layer through the real guest IAT. It connects to `127.0.0.1:12345`, sends `ping`, reads the echo, and exits 0. Start a one-shot echo server first:

```sh
python3 - <<'PY'
import socket
server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", 12345))
server.listen(1)
client, _ = server.accept()
client.sendall(client.recv(4))
client.close()
server.close()
PY
```

Then run the guest in another shell:

```sh
./run.sh guest/WINSOCK.EXE --run
```

Expected tail:

```text
target: guest/WINSOCK.EXE
imports: 8 functions from 2 DLLs
  WSOCK32.dll    7
  KERNEL32.dll   1
mapped 1048588 bytes; 8 IAT thunks installed; 8 imports have Rust handlers
guest exited with status 0x00000000 after 8 API calls
```

The source-tree test `./scripts/cargo.sh test --test winsock_vm` instead binds an ephemeral port and patches the guest image automatically. Genuine Winsock files in `guest/net/` are reference artifacts only; Rust handlers remain active.

## Boot the real-mode sector

The same binary contains a real-mode machine with BIOS `INT 10h` mode-set and teletype support:

```sh
./run.sh --boot
```

Expected output:

```text
boot sector: embedded:binaries/boot/OTWBOOT.BIN
VGA text buffer:
OTW BIOS boot OK
console: OTW BIOS boot OK

boot sector exited with status 0x00000000
```

To exercise the graphics path, select the embedded mode-13h sector and dump its framebuffer as an RGB24 PPM image:

```sh
./run.sh --boot --vga --video-dump /tmp/otw-frame.ppm
```

Expected output:

```text
boot sector: embedded:binaries/boot/OTWVGA.BIN
video: 320x200 Graphics320x200x256
video dump: /tmp/otw-frame.ppm
boot sector exited with status 0x00000000
```

The PPM is `320x200`, 8 bits per channel, with a `P6\n320 200\n255\n` header followed by 192,000 pixel bytes. It can be viewed with any PPM-capable image tool.

A live host window is available only in a separate, non-static development build:

```sh
./scripts/cargo.sh run --features display -- --boot --vga --window
```

This requires a display server (X11 today) and links X11/OpenGL dynamically; it is not part of the packaged binary. Close the window or press Escape to leave the event loop.

You can also supply a different 512-byte boot sector with a valid `0x55AA` signature:

```sh
./run.sh --boot /path/to/BOOT.BIN
```

## Run the minimal kernel

The same binary also contains a protected-mode machine for freestanding x86 kernels:

```sh
./run.sh --kernel
```

Expected output:

```text
kernel: embedded:binaries/kernels/HELLO.BIN
VGA text buffer:
Hello from the OTW minimal kernel!
serial: Hello from the OTW minimal kernel!
kernel exited with status 0x00000000
```

You can also supply a flat 32-bit kernel image:

```sh
./run.sh --kernel /path/to/KERNEL.BIN
```

To inspect the genuine target without executing it:

```sh
./run.sh --list-imports
```

Expected output starts with:

```text
target: embedded:binaries/win95/NOTEPAD.EXE
PE32 machine=0x014c base=0x00400000 entry=0x00401000 sections=6
imports: 127 functions from 5 DLLs
```

An explicit path overrides the embedded target:

```sh
./run.sh /path/to/OTHER.EXE
```

Execution requires `--run`:

```sh
./run.sh /path/to/OTHER.EXE --run
```

## Inspect genuine Paint artifacts

The package ships the genuine RTM Paint launcher and payload from the same verified media set as Notepad. The launcher runs cleanly:

```sh
./run.sh guest/PBRUSH.EXE --run
```

Expected tail:

```text
target: guest/PBRUSH.EXE
imports: 5 functions from 2 DLLs
mapped 1048588 bytes; 5 IAT thunks installed; 5 imports have Rust handlers
guest exited with status 0x00000000 after 5 API calls
```

The real payload is preserved for compatibility tracing, but it is not a supported full Paint guest:

```sh
./run.sh guest/MSPAINT.EXE --run
```

It now maps the included genuine `MFC30.DLL`, `MFCO30.DLL`, and `MFCANS32.dll` at their preferred bases, resolves their ordinal imports to real MFC code, and calls their `DllMain` before `AfxWinMain`. Paint runs interactively on the Rust Win32 layers; the static package has no display, so use a source build with `--features display` to see it (headless runs can be scripted with `OTW_INPUT`). No substitute binary is used. SHA-256 values are `8a46314c918bb130ad5f179a00bad0936a33543abefadbd64cfe732db6af36c8` for `PBRUSH.EXE` and `64069b69c18896236d1b00be3275690b62a1438c206ad90bdd539e557a78c936` for `MSPAINT.EXE`.

## List files on a FAT32 image

The read-only FAT32 parser is compiled into the static binary:

```sh
./run.sh --fat32 guest/disks/OTWDISK.IMG --list-files
./run.sh --fat32 guest/disks/OTWDISK.IMG --list-files Documents
```

Expected output starts with:

```text
fat32: guest/disks/OTWDISK.IMG label="OTW DISK" bps=512 spc=1 clusters=2012
fat32 listing: /
<DIR>          0  .
<DIR>          0  ..
<DIR>          0  Documents
              63  README.TXT
<DIR>          0  WINDOWS
             700  LONG.TXT
           1536  HELLO.EXE
          34304  NOTEPAD.EXE
           4608  PBRUSH.EXE
           2560  WINSOCK.EXE
```

The image is read-only and host-side; the package has no guest Win32 file APIs and no `display` feature, so the Explorer desktop requires a source build.

## Run inside a QEMU Linux VM

### Option A — share the package with 9p/virtiofs

Host:

```sh
qemu-system-x86_64 \
  -m 1024 \
  -hda linux.qcow2 \
  -virtfs local,path="$PWD/dist/otw-vm-0.1.0",mount_tag=otw,security_model=none
```

Guest:

```sh
mkdir -p /mnt/otw
mount -t 9p -o trans=virtio otw /mnt/otw
/mnt/otw/run.sh --hello --run
```

### Option B — copy the binary only

Because both guests, the kernel, and both boot sectors are embedded, a single file is enough:

```sh
scp dist/otw-vm-0.1.0/bin/otw-vm user@vm:/tmp/
ssh user@vm /tmp/otw-vm --hello --run
```

## Verify the package

Inside or outside the VM:

```sh
sha256sum --check SHA256SUMS
```

To confirm the binary really is self-contained, run it from an empty directory:

```sh
mkdir /tmp/otw-empty && cp dist/otw-vm-0.1.0/bin/otw-vm /tmp/otw-empty/
cd /tmp/otw-empty && ./otw-vm --hello --run
```

It should still print the embedded hello guest's output.

## Current limits and next work

- The x86 backend executes guests, all 127 of Notepad's imports have Rust handlers, and the Rust WinSock layer is active through guest IAT thunks. This static-pie package omits the `display` feature, so headless Notepad enters its message loop until externally stopped; the interactive virtual desktop with generic program icons, taskbar process list, dragging, resizing, activation/focus, per-window content, icon launching, and the FAT32 Explorer window requires a source build with `--features display`. Headless `--fat32 ... --list-files` works in this package, but the FAT32 layer is read-only and is not exposed through guest Win32 file APIs. Genuine `PBRUSH.EXE` runs as a launcher; genuine `MSPAINT.EXE` runs with real MFC modules (interactive with the display feature).
- `WriteFile` writes to host stdout; there is no full handle/console model yet.
- The boot machine implements only BIOS `INT 10h` mode set `03h`/`13h` and teletype; no PIC, keyboard, or disk BIOS yet.
- Boot graphics can be written to PPM files or shown in a static host window; keyboard input and guest-window dragging/resizing exist only in the optional process-level Notepad GUI runtime.
- The kernel mode boots flat 32-bit images only and does not chain from the boot sector.
- The Win32 mode is a flat process-level compatibility layer; there is no Windows segmentation, exception dispatch, native full window manager, or per-process isolation. The optional desktop is a small guest-facing window manager, not Win95's real USER32 window manager or GDI painting; its program icons launch unmodified binaries from the mounted FAT32 image, and its taskbar process list groups guest windows by class.
- Next milestone: finish the network path for genuine IE 3.0 once IE binaries are available, then broaden Winsock conformance and Paint's remaining dialogs (fonts, printing). See `win95-vm-architecture.md`.
