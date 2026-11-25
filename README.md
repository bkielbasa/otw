# Open Windows Together (otw)

An open-source reimplementation of Windows 95, written in Rust, that runs **unmodified Windows 95 programs** — the genuine Notepad and Paint, with the genuine MFC 3.0 DLLs they ship with — on Linux.

Programs run on an emulated x86 CPU ([Unicorn](https://www.unicorn-engine.org/)); everything they call is reimplemented: the kernel (processes, scheduling, structured exceptions), the Win32 API (KERNEL32, USER32, GDI32, common controls and dialogs, OLE, the C runtime, WinSock), and a Windows 95 style desktop shell. It starts as a process-level VM; booting on real hardware is a later goal.

## What works

- **Genuine programs, unmodified.** Notepad (all 127 imports) and Paint run their own code: Paint's tool box, colour box, menus, dialogs from its resources, drawing tools, flood fill, Edit Colors, cut/copy/paste through OLE, and opening and saving `.BMP` files. MFC's own C++ exception handling works, including errors thrown inside window procedures and caught further out.
- **A real window manager and graphics engine.** USER32 keeps a window tree with z-order, update regions, `WM_PAINT`, non-client frames, hooks, timers, menus, dialogs and the built-in controls. Windows minimize, maximize and restore from their caption buttons, the window menu (caption icon, Alt+Space, or right-click on the title bar, with keyboard Move and Size) and the taskbar; title bars show each program's own icon. GDI32 draws into per-window surfaces with raster operations, pens, brushes, regions, DIBs, palettes and a proportional font.
- **Installing modern Win32 programs.** The IrfanView 4.76 32-bit installer — a Unicode MFC wizard built for Windows XP — installs: its property-sheet wizard (with a checkbox list view) runs page by page, the program and its plugins unpack into `C:\Program Files\IrfanView`, its UTF-16 INI file and registry keys are written, and its Start Menu shortcuts are saved through `IShellLink`/`IPersistFile`. IrfanView itself then runs: it opens BMP, PNG and JPEG pictures from its command line or its Open dialog and shows them, with its menus, toolbar (`ToolbarWindow32`, image lists) and status bar. Unicode (`W`) APIs run on the ANSI implementation, as with the Microsoft Layer for Unicode.
- **Internet Explorer 5.** Its offline installer runs as on Windows 95: the setup wizard, the install engine extracting the CAB files, DCOM95, the ACME setup engine and INF installers as programs of their own, then the restart that registers IE's DLLs. The installed browser shows pages with its genuine MSHTML, from drive C: or from real web sites over plain HTTP (WinSock calls go to the host's network stack). This needed DLLs loaded at run time, threads, `CreateProcess`, a COM runtime, a persistent registry (`C:\WINDOWS\REGISTRY.REG`), the shell namespace, and cabinet (MSZIP, LZX) and INF support.
- **64-bit programs.** PE32+ images run in a 64-bit CPU: a program linked above 4 GiB is relocated below it, calls use the x64 convention, and structures with pointers (MSG, WNDCLASS, CREATESTRUCT, property sheets, COM vtables, ...) are translated to and from the subsystem's 32-bit layouts (`src/win32/x64.rs`). The 64-bit IrfanView 4.76 installer installs and the installed `i_view64.exe` shows pictures (`tests/irfanview_x64_setup.rs`); `binaries/guests/HELLO64.EXE` (`scripts/build-hello64-pe.py`) is a hand-built x64 test program.
- **Several programs at once.** Each program is its own process with its own memory, windows and message queue; programs take turns on the CPU whenever they wait for input. Copy in one, paste in another.
- **A desktop shell.** A taskbar with a notification area: programs' tray icons (`Shell_NotifyIconA`, with tooltips and click callbacks) and a clock. Start menu with cascading Programs folders built from real `.LNK` shortcuts, a folder window (File/Edit/View/Help menus, address bar, details list with programs' own icons), right-click menus, and file operations — new folder, new text document, rename, delete, cut/copy/paste, properties — with Windows keyboard shortcuts. Documents open in the program `WIN.INI` associates with them.
- **Structured exceptions** as in Windows NT: CPU faults (access violations, divide by zero) and `RaiseException` reach the program's `__try`/`__except` and C++ `catch` handlers.
- **Networking**: a WinSock 1.1 layer on top of host sockets.
- **Bare-metal experiments**: a tiny PC machine that boots OTW's own boot sectors and a freestanding kernel (`--boot`, `--kernel`, `make qemu`).

Not yet: printing and metafiles, HTTPS in IE (IE 5's SSL 3.0 and TLS 1.0 are refused by today's servers; plain-HTTP sites work), long-running programs that never wait for input (scheduling is cooperative; threads switch only when they wait), `ShellExecute` (only logs the request), 16-bit and DOS programs. 64-bit programs run with limits: below 4 GiB, without exception handling (no SEH unwinding yet), and only the structures listed in `src/win32/x64.rs` are translated.

## Getting started

Requirements: Rust (stable), a C toolchain with CMake for building Unicorn, Python 3 with `libclang` (or `LIBCLANG_PATH` pointing at libclang) for its bindings, and an X11 display for the desktop. `scripts/cargo.sh` sets up the build tools; the `Makefile` uses it.

```sh
make test          # build and run the tests
make run           # run the hand-built Hello guest: prints "Hello from the OTW Win95 VM!"
```

### Windows 95 files

Microsoft's files are not part of this repository. To run Notepad and Paint, copy them from your own Windows 95 (RTM) installation or media into `binaries/win95/`:

| File | SHA-256 |
|---|---|
| `NOTEPAD.EXE` | `0c90f92bedb3e437d0d9339219cb1f2898f44ed8172d88cf4bb62d29e9796f06` |
| `MSPAINT.EXE` | `64069b69c18896236d1b00be3275690b62a1438c206ad90bdd539e557a78c936` |
| `MFC30.DLL` | `ac51682ad37e7b6bf410ca5fda1661df2bb9bf699962f5ed2abae9253b919a23` |
| `MFCO30.DLL` | `fdd8f3e00ec70a1bf2fe3cede4513eeefd4cd4abdf78e880a610d3b502e5ff2f` |
| `MFCANS32.dll` | `cefcc02bc9980bb2cffb34e8cdf057772553186f2cb510c2c7225729ceffcd27` |

Other Win32 programs may work too; these are the ones the tests use. Tests that need these files are skipped when they are missing. `scripts/make-fat32-image.py` builds the demo disk image `binaries/disks/OTWDISK.IMG` from them.

### The desktop

```sh
make desktop
```

The desktop runs on drive C: — the host directory `~/.otw/C` (set `OTW_DRIVE_C` to use another), the same files programs open and save. The first run sets it up like Windows 95 Setup: the program files from `binaries/win95` go to `C:\WINDOWS`, Notepad and Paint shortcuts to `Start Menu\Programs\Accessories`, and file associations to `C:\WINDOWS\WIN.INI`. Existing files are never overwritten.

Start programs from the Start menu (click Start, Ctrl+Esc, or the Windows key), the Run dialog, or by opening documents. `make desktop DESKTOP_FAT32=binaries/disks/OTWDISK.IMG` browses a FAT32 disk image instead, read-only.

### A single program

```sh
make run TARGET=binaries/win95/NOTEPAD.EXE FEATURES=display ARGS="--run --window"
OTW_INPUT="w 2; s /tmp/paint.ppm" OTW_IDLE_EXIT=3 make run TARGET=binaries/win95/MSPAINT.EXE ARGS=--run   # headless, with a screenshot
```

Headless runs can be scripted and captured: `OTW_INPUT="w 2; m 120 100; d; m 200 180; u; s /tmp/shot.ppm" OTW_IDLE_EXIT=3` (commands: `m x y` move, `d`/`u` and `rd`/`ru` mouse buttons, `k vk [char]` key, `kd`/`ku` key down/up, `w n` wait, `s file` screenshot). `OTW_SHELL_INPUT` scripts the desktop the same way.

To try the IrfanView installer, download the 32-bit `iview476_setup.exe` into the repository root and run `make run TARGET=iview476_setup.exe FEATURES=display ARGS="--run --window"` (it installs into `C:\Program Files\IrfanView` on drive C:, `~/.otw/C` by default), or put it in the repository root (or `binaries/apps/`) and `tests/irfanview_setup.rs` installs it and opens a picture with it. To view a picture: `otw-vm ~/.otw/C/Program\ Files/IrfanView/i_view32.exe --run --window --args 'C:\PICTURE.JPG'` (`--args` passes a program its command-line arguments). The 64-bit `iview476_x64_setup.exe` installs too (`tests/irfanview_x64_setup.rs`).

To install Internet Explorer 5, unpack its offline package `ie5setup.zip` into a folder (`unzip ie5setup.zip -d ie5`) and run `otw-vm ie5/ie5setup.exe --run --window`. A program started from outside drive C: sees its folder as drive D:, as if it ran from a CD. In the wizard choose "Install Minimal, or customize your browser" and untick the components the package lacks: the 5.0 package from oldversion.com was downloaded without Windows Media Player and the Internet Connection Wizard, and "Install Now - Typical" includes both, so setup would try to download them from Microsoft's long-gone download sites. Then browse with `otw-vm ~/.otw/C/Program\ Files/Internet\ Explorer/IEXPLORE.EXE --run --window --args http://frogfind.com/`. `tests/ie5_setup.rs` installs it from `ie5setup.zip` in the repository root or `binaries/apps/`, then opens a page from drive C: and one from a web server on the loopback interface.

Debug switches: `OTW_TRACE_API` (every API call; `OTW_TRACE_RESULTS` adds return values), `OTW_TRACE_MODULES` (DLLs loaded and missing imports), `OTW_TRACE_COM`, `OTW_TRACE_REGISTRY`, `OTW_TRACE_SETUP` (INF and setup engine), `OTW_TRACE_THREADS` (threads and processes), `OTW_TRACE_NET` (socket connections), `OTW_TRACE_FAULTS` (registers at a guest fault), `OTW_WATCH=address` (writes to a guest address), `OTW_TRACE_MSG` (window messages), `OTW_TRACE_GDI`, `OTW_TRACE_CLIPBOARD`, `OTW_TRACE_EH` (exception dispatch), `OTW_TRACE_CODE` (recent instructions on a crash), `OTW_TRACE_FILES` (files opened), `OTW_DEBUG_WINDOWS` (window list at exit), `OTW_FAIL_API=Name=value` (make one API return a fixed value, to test error paths).

## How it is built

The system is layered; each layer uses only the ones below it, and `tests/layering.rs` enforces it.

| Layer | |
|---|---|
| `src/kernel/` | The OTW kernel: PE loading and process images, the CPU and processes, cooperative scheduling, the guest memory and system call interfaces, structured exception dispatch. It knows nothing about Win32. |
| `src/win32/` | The Win32 environment subsystem — the API DLLs in Rust — reached through the kernel's system call interface. |
| `src/vm.rs`, `src/session.rs` | A runnable program (a kernel process with Win32 system calls); several programs sharing a desktop. |
| `src/shell/` | The desktop shell and program launcher. |
| `src/host/` | The host window (`--features display`). |

Imports a program makes are bound to genuine DLL code where a genuine DLL (such as MFC) is present, and otherwise to thunks that the kernel turns into system calls. Rust calls back into guest code (window procedures, hooks, exception handlers) by nesting emulation.

More in [`docs/win95-vm-architecture.md`](docs/win95-vm-architecture.md) (architecture and status), [`docs/win95-api-dll-spec.md`](docs/win95-api-dll-spec.md), [`docs/win95-notepad-analysis.md`](docs/win95-notepad-analysis.md) and [`docs/packaging.md`](docs/packaging.md). The `scripts/` directory generates the hand-built test guests (`HELLO.EXE`, `WINSOCK.EXE`, `EHTEST.EXE`), boot sectors, kernel image, font and disk image.

## License

GPL-2.0-only (see [`LICENSE`](LICENSE)): the CPU backend statically links Unicorn Engine, which is GPL-2.0. The bitmap UI font in `binaries/fonts/` is rendered from DejaVu Sans (Bitstream Vera license, see `binaries/fonts/LICENSE.DejaVu`) by `scripts/build-font.py`.

This project is not affiliated with or endorsed by Microsoft. Windows is a trademark of Microsoft Corporation.
