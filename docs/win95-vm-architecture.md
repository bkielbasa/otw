# Windows 95 VM — architecture and current state

## Goal

Run genuine Win95 RTM programs on an open-source Rust implementation of the Win95 API surface. The desktop is driven by the mounted FAT32 image so any 32-bit PE placed on that image can be launched; Notepad remains the first fully traced compatibility target. The current application track is also the host-backed WinSock stack needed to run genuine Internet Explorer 3.0. The VM comes first; real hardware is a later phase. Microsoft binaries are compatibility-test artifacts only.

## Current state

The VM has a working x86 CPU backend and executes both a hand-built Win32 `HELLO.EXE` and the genuine Win95 RTM `NOTEPAD.EXE`. With the optional `display` feature, a Win95-style virtual desktop scans the mounted FAT32 image and creates one generic icon per executable; it does not contain a fixed Notepad/Paint shortcut list. Double-clicking an icon queues a launch that reads that binary's genuine bytes from the image, discovers its DLL imports, and runs the PE in a fresh guest VM. The direct `--run --window` path still runs the genuine Notepad binary itself. Guest windows are drawn from their own geometry with dragging, resizing, z-order, focus/activation, and per-window edit content. Genuine guests complete their real WinMain/init paths, run their own message loops when applicable, and receive keyboard input through the active window's built-in EDIT child. Programs started from the desktop run at the same time, each in its own process (see "Several programs at once"). All 127 Notepad imports have Rust handlers.

The loader is now multi-module. Genuine companion DLLs found next to the target are parsed for exports and mapped at their PE preferred bases; ordinal and named imports that resolve inside that module set call real DLL code, while remaining imports receive synthetic IAT thunks backed by Rust handlers. Each companion DLL's `DllMain` is called with `DLL_PROCESS_ATTACH` before the executable starts. Unicorn's 32-bit `FS_BASE` is ignored, so the guest TIB is exposed through a real GDT entry with `FS` pointing at `TIB_BASE`. `WSOCK32.DLL`, `WINSOCK.DLL`, and `MSVCRT20.DLL` are deliberately excluded from companion mapping so their Rust-backed handlers remain active; `src/shell/launch.rs` applies the same exclusion when discovering DLLs for a desktop-launched binary.

Genuine Paint artifacts from the same verified English RTM 3.5-inch media set are preserved as compatibility tests. `PBRUSH.EXE` is the small launcher: it imports 5 functions from KERNEL32/SHELL32, all of which have handlers, and exits with status `0x00000000` after 5 API calls. Its SHA-256 is `8a46314c918bb130ad5f179a00bad0936a33543abefadbd64cfe732db6af36c8`. The real payload `MSPAINT.EXE` (SHA-256 `64069b69c18896236d1b00be3275690b62a1438c206ad90bdd539e557a78c936`) now maps genuine `MFC30.DLL`, `MFCO30.DLL`, and `MFCANS32.dll` at their preferred bases, resolves their ordinal imports to real MFC code, and calls `DllMain` before `AfxWinMain`. It runs to an interactive UI on the Rust USER32/GDI32/MSVCRT20/OLE32 layers: Paint's own code draws every window, and drawing, menus, dialogs, scroll bars and BMP open/save work (see `tests/paint_vm.rs`). No substitute binary is used.

A read-only FAT32 reader, host-side Explorer, and generic launcher are also implemented. `src/fat32.rs` parses BPB geometry, FAT chains, short and long directory entries, and path normalization; `src/shell/launch.rs` turns its executable entries into desktop shortcuts and builds VMs from their bytes and imports. The optional GUI draws generic file icons, an Explorer window, and a shell taskbar entry backed by a mounted raw image. Double-clicking an executable icon or Explorer row launches that file. The headless CLI lists files from the default/static build. No guest Win32 file API is backed by this layer yet.

The network stack is a Rust WinSock 1.1 layer (`src/win32/winsock.rs`) backed by host `std::net`, not a guest TCP/IP reimplementation and not a real `WSOCK32.DLL` mapping. It handles startup/cleanup, TCP and UDP sockets, connect/bind/listen/accept, send/receive, `select`, `getpeername`/`getsockname`, byte-order helpers, host/protocol/service lookups, blocking-hook stubs, and `WSAAsyncSelect`/async lookup message posting into the guest message queue. The hand-built `binaries/guests/WINSOCK.EXE` connects to a host loopback server, sends `ping`, reads the echo, and exits 0 through the real guest IAT path. Genuine Winsock artifacts from the verified RTM media are preserved under `binaries/win95/net/` with checksums, but are intentionally not mapped. Internet Explorer 3.0 is the target application; the RTM floppy set does not include the IE binaries, so IE tracing waits for genuine artifacts.

The separate machine layer can boot a 512-byte real-mode sector, service BIOS `INT 10h` mode-set and teletype services, program the VGA DAC, dump a 320x200x256 frame as a PPM image, and show it in a host window with a live event loop.

Components:

### Layers

The code is layered; each layer uses only the layers below it, and `tests/layering.rs` fails on any reference upwards (and on Win32 names inside the kernel).

1. **`src/kernel/` — the OTW kernel.** Everything about running a program that does not depend on what it calls:
   - `pe.rs` reads PE32 images; `loader.rs` maps an executable and its genuine DLLs at their preferred bases and binds imports to DLL exports, consulting an `ImportPolicy` for imports the subsystem must provide itself; `iat.rs` gives every other import a thunk; `image.rs` is the resulting process `Image` (modules, thunks, command line), including thunks for the subsystem's extra entry points and the kernel's own `KiUserExceptionDispatcher`.
   - `memory.rs` is the guest address-space interface (`GuestMemory`: read/write, call guest code, request a resume, yield) and layout; `syscall.rs` is the system call interface — `SystemCalls` (argument count and call, given a `&mut dyn GuestMemory`) — through which a process reaches its environment subsystem.
   - `cpu.rs` runs a `Process` on Unicorn: maps the image, stack, heap, TIB and GDT, runs DLL initialisation, turns thunk calls into system calls, nests guest callbacks, delivers CPU faults as exceptions, and stops at a yield. `scheduler.rs` gives yielding processes turns cooperatively. `eh.rs` is structured exception dispatch: raising (software exceptions take the same on-stack record/context path as faults), walking the `FS:[0]` chain, unwinding, and resuming.
2. **`src/win32/` — the Win32 environment subsystem.** The API DLLs as Rust: `mod.rs` (the per-process `ApiContext` and `SharedState`, DLL routing, `impl SystemCalls`, `Win32Imports`), `kernel32.rs`, `user.rs`, `gdi.rs`/`gfx.rs`, `controls.rs`, `menu.rs`, `commdlg.rs`, `clipboard.rs`, `ole.rs`/`storage.rs`, `crt.rs` and `cxx.rs` (MSVCRT20, including the C++ and `__try` frame handlers), `winsock.rs`.
3. **`src/vm.rs`** — a runnable program: `Image::load` with the Win32 policy and entry points, plus an `ApiContext`; `run` / `start`.
4. **`src/session.rs`** — a desktop session: programs side by side on the kernel scheduler, each with its own Win32 state; the foreground z-order and the shared clipboard.
5. **`src/shell/`** — the desktop shell (see "Desktop shell"); `launch.rs` finds programs and their DLLs on a volume and starts them.
6. **`src/host/`** — the host window (`display.rs`, optional `display` feature).

Outside the layers: `src/machine.rs` (the bare-metal PC machine for boot sectors and freestanding kernels) and `src/fat32.rs` (read-only FAT32 reader).
- `src/main.rs` — CLI: `./scripts/cargo.sh run -- [--hello | target] [--run] [--list-imports] [--window]`, `./scripts/cargo.sh run --features display -- --desktop [--fat32 DISK.IMG]`, `./scripts/cargo.sh run -- --kernel [path/to/KERNEL.BIN]`, `./scripts/cargo.sh run -- --boot [--vga] [path/to/BOOT.BIN] [--video-dump FILE] [--window]`, `./scripts/cargo.sh run -- --fat32 DISK.IMG --list-files [PATH]`, and `./scripts/cargo.sh run --features display -- --fat32 DISK.IMG --explorer`. `--desktop`/`--explorer` call `scan_executables()` on the mounted image and pass the results to the GUI. Direct PE runs discover genuine companion DLLs next to the target, skipping `WSOCK32.DLL`, `WINSOCK.DLL`, and `MSVCRT20.DLL`.

## CPU backend contract

The loader stays backend-agnostic. The x86 backend implements this contract:

1. Map every module's flat guest memory at its PE preferred base, plus a 1 MiB stack at `0x00100000`, a bump heap, and the synthetic thunk region. Reserved high addresses such as `TIB_BASE` and `GDT_BASE` must not overlap companion images.
2. Install a real GDT and point `FS` at the guest TIB/TIB-compatible structure; Unicorn 2.1.5 ignores `FS_BASE` in 32-bit mode.
3. Set `ESP` near the top of the stack. Before starting the EXE at `EIP = image.entry_va()`, call each companion DLL's `DllMain` with `DLL_PROCESS_ATTACH` and stop on a sentinel return address.
4. Map the synthetic thunk region as executable memory. Each thunk contains `ret` as a fallback, but a code hook intercepts it before it executes.
5. On a thunk call, read the return address and `stdcall` arguments from the guest stack, query the API's argument count, dispatch into the Rust layer with a guest-memory view, write EAX, pop the return address and arguments, and set EIP back to the caller. MSVC CRT imports are `cdecl`: handlers read known arguments but leave stack cleanup to the guest.
6. Treat `ExitProcess` specially: record the guest exit code, write EAX, and stop emulation.
7. Report invalid guest memory accesses through a memory hook instead of silently continuing.

## Real-mode boot stage and VGA framebuffer

`--boot` selects a real-mode machine instead of the Win32 process loader. It maps 640 KiB of low memory, the VGA text buffer at `0xB8000`, and the mode-13h framebuffer at `0xA0000`, then loads a 512-byte boot sector at `0x0000:7C00`. It starts with `CS=DS=ES=SS=0`, `IP=SP=0x7C00`.

Implemented BIOS video services:

- `INT 10h/AH=0x00`, `AL=0x03`: 80x25 text mode.
- `INT 10h/AH=0x00`, `AL=0x13`: 320x200x256-color graphics mode.
- `INT 10h/AH=0x0E`: teletype output in text mode. It writes `AL` at the current VGA cursor, treats `\r` as carriage return and `\n` as line feed, scrolls after 25 rows, and mirrors printable bytes into the host's console capture.

The DAC is emulated through ports `0x3C8` (index) and `0x3C9` (RGB data). The default palette is identity/grayscale, so pixel value `N` renders as RGB `(N, N, N)` unless the guest reprograms an entry. `OUT 0xF4, AL` still stops emulation with an exit status, so no disk or PIC is needed for this stage.

`scripts/build-bootsector.py` emits `binaries/boot/OTWBOOT.BIN`, a text-mode sector that prints `OTW BIOS boot OK` and exits with status 0:

```sh
./scripts/cargo.sh run -- --boot
```

```text
boot sector: binaries/boot/OTWBOOT.BIN
VGA text buffer:
OTW BIOS boot OK
console: OTW BIOS boot OK

boot sector exited with status 0x00000000
```

`scripts/build-vga-bootsector.py` emits `binaries/boot/OTWVGA.BIN`, a graphics-mode sector that programs DAC entry `0x2A` to red and draws three pixels. Use `--vga` to select the embedded/default image and `--video-dump` to save an RGB24 PPM file:

```sh
./scripts/cargo.sh run -- --boot --vga --video-dump /tmp/otw-frame.ppm
```

```text
boot sector: binaries/boot/OTWVGA.BIN
video: 320x200 Graphics320x200x256
video dump: /tmp/otw-frame.ppm
boot sector exited with status 0x00000000
```

The PPM header is `P6\n320 200\n255\n` followed by 192,000 bytes of RGB pixels.

The same frame can be shown in a host window. Build with the optional `display` feature and pass `--window`:

```sh
./scripts/cargo.sh run --features display -- --boot --vga --window
```

`src/host/display.rs` (compiled only when `display` is enabled) creates a winit window, a GLX OpenGL context via glutin-winit, and a glow RGB texture drawn as a fullscreen triangle strip. The ApplicationHandler event loop processes `Resized`, `RedrawRequested`, `CloseRequested`, and Escape. Window scale is chosen by an integer factor capped at 1600x1000, and the texture uses nearest-neighbour filtering to keep the pixelated look.

Because the guest currently runs to completion before a frame is available, the window shows a static snapshot. Live frame updates need sliced emulation (see below). Without the feature, `--window` prints a rebuild hint. The packaged binary does not enable `display`, so it stays static-pie and dependency-free.

### Live Win32 desktop

For `--run --window` and `--desktop`, the same OpenGL surface is a 1024x700 Win95-style desktop rather than a single guest window. `GuiState.windows` is the source of truth: the renderer draws every visible top-level window at its own `x/y/width/height`, with a frame, title bar, class-based menu strip, client area, close box, and resize grip. The teal desktop has one generic file icon per executable found by `scan_executables()`, plus Explorer when a FAT32 image is mounted; no icon name is hardcoded in the renderer. The bottom taskbar has a Start button plus a process list built by grouping visible top-level windows by class; the active group's button and title bar are highlighted. Each window's client text comes from its own descendant EDIT control via `GuiState::client_text_for`, so separate windows do not share text state. Host `Resized` events only resize the surface and clamp windows; they no longer inject a misleading `WM_SIZE` into the guest. Initial `WM_SIZE` messages are derived from each guest window's client rectangle.

Hit testing checks close box, resize grip, title bar, client area, taskbar process button, and desktop icon in that order. Title-bar dragging updates `GuestWindow.x/y` with the grab offset, clamps the window inside the desktop and above the taskbar, and queues `WM_MOVE`. Resize-grip dragging updates `width/height` with minimum and desktop-derived maximum sizes and queues `WM_SIZE`. Both mark the desktop dirty for redraw; mouse-up releases cursor confinement and logs the final geometry.

Pressing any part of a window calls `GuiState::activate_window`: the previous top-level gets `WM_ACTIVATE(WA_INACTIVE)` and loses focus, the new window is moved to the end of the z-order vector, gets `WM_ACTIVATE(WA_ACTIVE)`, and focus moves to its EDIT child with `WM_SETFOCUS`/`WM_KILLFOCUS`. `GetFocus`, `SetFocus`, `GetActiveWindow`, `SetForegroundWindow`, and `SetActiveWindow` now use this shared state. Keyboard input goes to `focus_hwnd` as `WM_CHAR`. Destroying a window also removes its children, clears active/focus state when needed, and activates the next visible top-level window. The close box and host close button both enqueue guest `WM_CLOSE`; the guest's own WndProc then runs its shutdown path.

The taskbar process list is derived from `GuiState` rather than from a host process table: visible top-level windows with the same class are grouped into one `ProcessEntry`, the active group is highlighted, and clicking a group activates its most recently created window. A mounted FAT32 volume adds one host-side `Explorer` entry ahead of the guest entries; clicking it restores the shell window and logs `[gui] Explorer taskbar entry activated`. Double-clicking a program icon records a `LaunchRequest` instead of creating a host-drawn shell window. `GuiRuntime::pump_until_closed()` consumes that request and calls `launch()`, which resets per-process state, builds a VM from the selected file, runs it, and updates the status/title bar. `.COM` files are recognized and reported as unsupported.

### Desktop shell

`src/shell/` is the host-side Win95-style shell, independent of the display backend so it is unit-tested headlessly. `Shell` takes pointer and key input (`pointer_down/up/move`, `wheel`, `key`), paints into a `gfx::Surface` with the same `Painter`, font and system colours as guest window chrome, and returns `LaunchRequest`s (program path plus command-line arguments). `display.rs` interleaves its paint layers with program windows (desktop and background folder window, program windows, foreground folder window, then taskbar, menus and dialogs) and routes input to it first: anything the shell does not claim goes to the emulated window manager. `volume.rs` abstracts drive C: (the host directory behind `crt::drive_root`, writable, case-insensitive like FAT) and FAT32 images (read-only). `popup.rs` is the cascading menu engine shared by the Start menu, the folder window's menu bar and context menus. `lnk.rs` reads and writes Win95 shell links; `icons.rs` has the built-in icon set and decodes `RT_GROUP_ICON`/`RT_ICON` resources from program files; `setup.rs` performs first-run setup and reads `WIN.INI [Extensions]` associations. The shell refreshes its views when a program exits.

### Several programs at once

Each program started from the desktop is a kernel `Process` with its own CPU and its own Win32 `SharedState` — address space, heap, handles, windows, message queue — so programs cannot see each other's state. They share the host thread cooperatively: when a program's `GetMessage`/`WaitMessage` would wait at its top level, the process yields (the CPU stops at a dedicated address and the call is made again on the next turn), and the scheduler gives every process a turn each round. A program waiting inside a modal loop (a dialog box or menu) cannot yield, so its waits run the other programs from inside the loop; a process is never resumed from inside itself. The session keeps the programs' z-order: clicking a program's window or taskbar button brings it to the front, and the program that lost the front gets `WM_NCACTIVATE`/`WM_ACTIVATE`/`WM_ACTIVATEAPP(FALSE)`. The desktop composites every program's windows back to front and routes input to the program under the pointer (or the foreground one, for keys). Clipboard contents are copied between programs around their turns, so text and pictures copied in one program paste in another.

### Programs built for later Windows

The IrfanView 4.76 32-bit installer (`tests/irfanview_setup.rs`) is the first program built for Windows XP that runs: it is a Unicode, statically linked MFC wizard. What it needed:

- **Unicode APIs** (`src/win32/unicode.rs`), the Microsoft Layer for Unicode approach: a `W` function converts its strings to ANSI, calls the `A` implementation and converts the results back; window procedures registered through `W` functions receive `WM_SETTEXT`/`WM_GETTEXT`/`WM_CREATE` and list-box strings as UTF-16.
- **XP-era KERNEL32** (`src/win32/xp.rs`): performance counters, `SetUnhandledExceptionFilter`, pointer encoding, `RtlUnwind`, code-page information.
- **COMCTL32** (`src/win32/comctl32.rs`): property sheets and wizards (`PSN_*` notifications, `PSM_*` messages, page callbacks, MFC's CBT-hook subclassing of pages) and a report-view `SysListView32` with checkboxes.
- **Profiles** (`src/win32/profile.rs`): `GetPrivateProfileString` and friends read and write real `.ini` files on drive C:, keeping UTF-16 files in UTF-16.
- **Shell links** (`src/win32/shelllink.rs`): `CoCreateInstance(CLSID_ShellLink)` returns an object with `IShellLinkA`, `IShellLinkW` and `IPersistFile` that saves the `.LNK` files the shell reads (`src/win32/lnk.rs`).
- **A per-process current directory**: relative file names resolve against it.
- Delay-loaded DLLs (SHLWAPI, SHELL32, OLE32, OLEACC, WINSPOOL) resolve through `GetProcAddress`, by name or ordinal.

The installer's image reaches past 0x500000, where the command line used to be stored; the command line now goes after the image when the image is that large, since writing it there had corrupted the installer's embedded payload.

### Internet Explorer 5

Internet Explorer 5 installs from its offline package with its own setup and then browses (`tests/ie5_setup.rs`). Much of Windows had to be there for it:

- **DLLs loaded at run time** (`src/win32/module.rs`): `LoadLibrary` finds a DLL on drive C: the way Windows searches, maps it (relocating it when its preferred base is taken, `kernel::loader::relocate`), binds its imports to other genuine DLLs or to the subsystem (the subsystem's own DLLs, such as KERNEL32 and OLE32, are never loaded from disk), and calls its DllMain. `GetProcAddress` follows export forwarders. A genuine COMCTL32 found on the drive replaces the subsystem's common controls.
- **Threads** (`src/kernel/cpu.rs`, `src/win32/sync.rs`): a process's threads share its CPU and address space, each with its own stack, TIB and TLS. They switch only when the running one waits: a thread that waits on something not yet signalled comes off the CPU (the waiting call is made again when it next runs), and the first thread gives the other threads the CPU while it waits for messages or objects. Events, mutexes, semaphores, critical sections and thread handles have their real semantics; new threads send `DLL_THREAD_ATTACH`.
- **Programs starting programs**: `CreateProcess` asks the session's launcher (`vm::NestedLauncher`), which runs the child to its end inside the call, sharing the display and the rest of the scripted input; installers wait for their helpers anyway. `RUNDLL32.EXE` is built in (a stub program whose entry point is a subsystem function).
- **The registry** (`src/win32/registry.rs`): one tree for all programs, kept in `C:\WINDOWS\REGISTRY.REG` (REGEDIT4), with ANSI and Unicode functions; `HKEY_CLASSES_ROOT` is `HKLM\SOFTWARE\Classes`.
- **COM** (`src/win32/com.rs`): `CoCreateInstance` and `CoGetClassObject` find in-process servers through `CLSID\{...}\InprocServer32`, load them and ask `DllGetClassObject`; registered class objects, per-thread `CoSetState`, bind contexts (`src/win32/bindctx.rs`).
- **The shell namespace** (`src/win32/shellfolder.rs`): the desktop `IShellFolder`, file-system folders below it, item enumeration and ID lists; class items (IE's Internet folder) bind to their COM class. SHELL32's ordinal-only exports (`src/win32/shell.rs`).
- **Setup machinery**: an INF engine (`src/win32/inf.rs`: strings, LDIDs and custom destinations, `AddReg`/`DelReg`, `CopyFiles`, `RegisterOCXs`, setup commands) behind ADVPACK (`src/win32/advpack.rs`: `RegInstall`, `RunSetupCommand`, `LaunchINFSection`, `ExecuteCab`, `ExtractFiles`), and cabinet files with MSZIP and LZX compression, multi-volume sets included (`src/win32/cab.rs`). A restart requested with `ExitWindowsEx` applies `WININIT.INI` and runs the `RunOnce` commands.
- Smaller pieces: VERSION.DLL, RPCRT4 proxy registration, RICHEDIT (as a plain edit control), a tree view, MENUEX resources, `SPI_GETNONCLIENTMETRICS`, window properties named by atom, and a Windows 95 `GetVersion`.

The browser itself is IE's genuine code: SHDOCVW and BROWSEUI build the frame, URLMON and WININET fetch, MSHTML parses and renders on its worker threads. For pages from the web:

- **WinSock** (`src/win32/winsock.rs`) is backed by host sockets. WinINet imports WSOCK32 by ordinal, binds a loopback datagram socket to itself to wake its `select` thread, and connects non-blocking stream sockets. `select` takes the calling thread off the CPU until a socket is ready or the timeout passes, so the program's other threads go on.
- **Message queues per thread**: a window's messages, timers and paints go to the thread that created it, as do `PostQuitMessage`'s. URLMON posts its download notifications to a window of the thread that started the download, and only that thread's window procedure finds the download in its thread state.
- **The VMM's page services**: MSHTML manages its heap with `VxDCall` (KERNEL32 ordinals 1-9) `PageReserve`/`PageCommit`/`PageFree`, served from the subsystem's virtual memory (`misc::vxd_call`).
- Faults on any thread raise exceptions through that thread's SEH chain and stack.
- OLE32 serves the component categories manager (`src/win32/catmgr.rs`) that ActiveX controls register with; DCOM95's COMCAT.DLL forwards to it.
- A restart runs `RunOnce` again while IE's `RunOnceEx` list is unfinished: its processor stops for another restart when a DLL it registered must replace one in use.

### Read-only FAT32 Explorer

`src/fat32.rs` reads a raw FAT32 image as an owned byte vector. Boot-sector checks cover the `0x55aa` signature, bytes/sector and sectors/cluster, reserved and FAT regions, root cluster bounds, and declared volume length. Directory reading validates LFN checksums, filters volume labels and deleted entries, decodes OEM short names, and follows cluster chains while rejecting loops or out-of-range clusters. The API surface is deliberately small: `list(path)` and `read_file(path)` (plus `read_file_entry` for rows already in hand). There is no write path, host mount, or guest `CreateFileA`/`ReadFile` integration yet.

Headless listing is available in every build:

```sh
./target/debug/otw-vm --fat32 binaries/disks/OTWDISK.IMG --list-files
./target/debug/otw-vm --fat32 binaries/disks/OTWDISK.IMG --list-files Documents
```

`scripts/make-fat32-image.py` regenerates the deterministic 4,194,304-byte demo image `binaries/disks/OTWDISK.IMG` (SHA-256 `40123c695b8da6ab6d6716b5c0e888e68be37a88a36dd671c351e9221ad2e865`). It contains `README.TXT`, `LONG.TXT`, `Documents/NOTES.TXT`, `WINDOWS/SYSTEM.INI`, and real launchable PE files `HELLO.EXE`, `NOTEPAD.EXE`, `PBRUSH.EXE`, `MSPAINT.EXE` (plus `MFC30.DLL`, `MFCO30.DLL`, `MFCANS32.DLL`) and `WINSOCK.EXE`, with long filenames stored as real LFN entries.

With `display` enabled, `--fat32 ... --explorer` runs a shell-only desktop and scans the image for program icons; `--fat32 ... --window` adds the Explorer beside a running guest. `ExplorerState` holds the mounted volume, current path, sorted rows, selection, and status text. The renderer draws a folder desktop icon, a Win95-style frame with menu/toolbar/address/list/status areas, small folder/file row icons, and the taskbar button. Pointer hit testing covers close, Up, rows, title bar, and client; double-clicking a row opens directories, reads non-executable files, or launches executables, and Enter/Backspace/arrow keys mirror that navigation. A launch is queued out of the callback and executed by `GuiRuntime::launch`; opening or closing the shell window updates the taskbar entry.

## Machine layer for freestanding kernels

`--kernel` selects the protected-mode machine layer instead of the Win32 process loader. It boots a flat 32-bit binary at `0x00100000` with:

- 640 KiB of low memory (`0x00000000`–`0x0009FFFF`)
- a flat GDT at `0x500` with ring-0 code selector `0x08` and data selector `0x10`
- `CR0.PE` set, paging disabled, stack top at `0x00200000`
- VGA text buffer at `0xB8000` (80×25)
- COM1 (`0x3F8`) output captured by the host
- debug-exit port (`0xF4`) to stop emulation with a status code

`scripts/build-kernel.py` emits `binaries/kernels/HELLO.BIN`. It is a 546-byte flat binary that writes a banner to VGA with attribute `0x1F`, echoes it to COM1, then writes 0 to the exit port.

Run it with:

```sh
./scripts/cargo.sh run -- --kernel
```

Expected output:

```text
kernel: binaries/kernels/HELLO.BIN
VGA text buffer:
Hello from the OTW minimal kernel!
serial: Hello from the OTW minimal kernel!
kernel exited with status 0x00000000
```

This is a separate protected-mode test: it does not use the real-mode boot stage or BIOS. It validates protected-mode segmentation, memory-mapped VGA, and port I/O; the boot stage above validates real-mode execution and BIOS video services.

## Hello World guest

`scripts/build-hello-pe.py` emits `binaries/guests/HELLO.EXE` without a cross-compiler. It is a genuine PE32 i386 console image with one `.text` section and three KERNEL32 imports. The code calls:

```asm
push STD_OUTPUT_HANDLE
call [GetStdHandle]
push 0
push &bytes_written
push message_len
push message
push eax
call [WriteFile]
push 0
call [ExitProcess]
```

Run it with:

```sh
./scripts/cargo.sh run -- --hello --run
```

Expected tail of the output:

```text
Hello from the OTW Win95 VM!
guest exited with status 0x00000000 after 3 API calls
```

## WinSock guest stack

`src/win32/winsock.rs` implements a WinSock 1.1 guest ABI over host `std::net`. It is a Rust API layer, not a guest TCP/IP reimplementation and not a mapping of the genuine `WSOCK32.DLL`. The guest still sees WinSock handles, `SOCKET_ERROR`/`INVALID_SOCKET`, WSA error codes, and Win95 structure layouts. `WSAStartup` is stdcall with two arguments; getting that count wrong corrupts the guest stack.

Implemented groups:

- lifecycle and errors: `WSAStartup`, `WSACleanup`, `WSAGetLastError`, `WSASetLastError`, blocking-hook stubs;
- sockets: `socket`, `closesocket`, `bind`, `listen`, `accept`, `connect`, `getpeername`, `getsockname`, `shutdown`, `ioctlsocket`, `__WSAFDIsSet`;
- data and readiness: `send`, `recv`, `sendto`, `recvfrom`, `WSARecvEx`, `select`;
- name services: `gethostname`, `gethostbyname`, `gethostbyaddr`, `getprotobyname`, `getprotobynumber`, `getservbyname`, `getservbyport`, `inet_addr`, `inet_ntoa`, `htonl`, `ntohl`, `htons`, `ntohs`;
- async notifications: `WSAAsyncSelect`, `WSAAsyncGetHostByName`, `WSAAsyncGetHostByAddr`, `WSACancelAsyncRequest`.

`WSAAsyncSelect` records hwnd/message/events on the socket and posts `GuestMessage` values into `GuiState.posted` when connect, write, read, accept, or close events occur. Async host lookups fill a guest `HOSTENT` into the supplied buffer, post the completion message, and return a non-zero async handle. Unimplemented name-service provider APIs (`EnumProtocols*`, `GetAddressByName*`, and similar) are not claimed.

`scripts/build-winsock-pe.py` emits `binaries/guests/WINSOCK.EXE` without a cross-compiler. It imports seven functions from `WSOCK32.dll` plus `ExitProcess`, calls `WSAStartup`, opens a TCP socket, connects to `127.0.0.1`, sends `ping`, verifies the echo, closes the socket, and exits 0. `tests/winsock_vm.rs` binds an ephemeral listener, patches `sin_port` in the guest image, and runs the whole path through the loader and IAT:

```sh
./scripts/cargo.sh test --test winsock_vm -- --nocapture
```

Genuine Winsock artifacts from the same verified English RTM set are preserved in `binaries/win95/net/` (`SHA256SUMS` is alongside them):

| Artifact | SHA-256 |
|---|---|
| `wsock32.dll` | `2dbc73f5512d5a293411b70db18cc7d4ab0c21637fb905eaf38741413550b78e` |
| `winsock.dll` | `e7dddad7df6c82b9b9f54311a55095a96a783768d7d29457bb5c5a93e21e0fc2` |
| `wsock.vxd` | `e45de95511b8cc3505f35e69735072907f74b82ee57ee946a7bfd1f61a862043` |
| `inetmib1.dll` | `6a9a9adb46a4d8904a8434fe416bcaf85b6f561fd8788070d0a39d79c66e8155` |

They are compatibility/reference artifacts only. `api::is_rust_backed_dll()` keeps `WSOCK32.DLL` and `WINSOCK.DLL` out of companion mapping, so real IE/WININET-era imports will reach the Rust handlers instead of genuine DLL code.

## Build requirements

`unicorn-engine` builds its C sources and generates bindings at build time. Use `scripts/cargo.sh`, which:

- Adds a Python environment containing `cmake`, `ninja`, and `libclang` to `PATH` when one is available.
- Sets `LIBCLANG_PATH` from `clang.cindex` when possible.
- Sets `BINDGEN_EXTRA_CLANG_ARGS` to `.cargo/clang-include`, which contains minimal `stdbool.h`, `stddef.h`, `stdarg.h`, and `stdint.h` shims for libclang distributions without a full resource directory.

The x86 backend is GPL-2.0 (Unicorn), so the crate and packaged binary are licensed GPL-2.0-only.

## Verification

```sh
./scripts/cargo.sh test
```

Current result: 33 tests pass in the default build, covering the FAT32 parser, generic program scan and VM construction from the disk image, Notepad loader/IAT, Hello World, text boot, VGA framebuffer, protected-mode kernel, Rust-backed DLL exclusion, Winsock unit APIs, and the hand-built Winsock guest's loopback TCP round trip. With `--features display`, 53 tests pass in total; the extra tests cover desktop geometry, generic icon hit-testing and launch requests, Explorer executable/file rows, guest and shell taskbar entries, title-bar/close/resize hit-testing, drag clamping, `WM_MOVE`/`WM_SIZE` packing, per-window text, activation/focus/z-order, FAT32 browsing/reading, Explorer hit testing, and icon/window/taskbar rendering.

Manual GUI verification (requires X11):

```sh
./scripts/cargo.sh build --features display --offline
DISPLAY=:0 ./target/debug/otw-vm --run --window
```

Confirm a window titled `OTW Desktop — Untitled - Notepad` appears with the teal desktop, a bottom taskbar process entry, and (with `--fat32`) one generic icon per executable on the image. For a shell-only desktop, run `DISPLAY=:0 ./target/debug/otw-vm --desktop --fat32 binaries/disks/OTWDISK.IMG`; the title becomes `OTW Desktop — Explorer — \`, with `HELLO.EXE`, `NOTEPAD.EXE`, `PBRUSH.EXE`, and `WINSOCK.EXE` icons plus Explorer. Double-click a program icon and confirm the title becomes `OTW Desktop — Running HELLO.EXE` (or another selected program) and later `... exited with status 0x0 after N API calls`; double-click Explorer and confirm `Explorer — \`. Clicking a taskbar entry reactivates its window. Left-drag a title bar and confirm `[gui] moved guest window ... to (x, y)` appears; drag the hatched bottom-right grip and confirm `[gui] resized guest window ... to WxH` appears. Clicking another guest window should change the title-bar colour, z-order, and taskbar highlight while typing continues in the newly focused EDIT child. Click the drawn `x` close box or the host window close button and confirm `guest exited with status 0x00000000`.

For the shell-only Explorer desktop:

```sh
DISPLAY=:0 ./target/debug/otw-vm --fat32 binaries/disks/OTWDISK.IMG --explorer
```

Confirm the folder icon, `Explorer` taskbar button, and window title `Explorer — \`. Double-click the folder icon and look for `[gui] Explorer desktop icon opened FAT32 volume`; double-click `Documents` and confirm the address becomes `\Documents` with `NOTES.TXT` in the list. Double-clicking a file should report bytes read in the status bar; double-clicking `HELLO.EXE` should report `Launching HELLO.EXE` and then launch it. Use `Up` to return to the root, click the taskbar button to reactivate the window, and close with `x`, Escape, or the host close button. With a PE guest plus `--fat32 ... --window`, confirm both guest and Explorer taskbar entries coexist and the Explorer window draws above the guest window.

## Known placeholders

- The API layer covers all 127 imports, but many are minimal handlers rather than complete Win95 implementations: common dialogs generally return cancel/no-op, `MessageBoxA` logs to the host, and print/shell/help paths are compatibility stubs. Startup, the main message loop, edit-control input, and the close/quit path have been exercised; every menu and dialog path has not.
- `GetStartupInfoA` writes a minimal zeroed `STARTUPINFOA`; the direct CLI defaults the guest command line to `NOTEPAD.EXE`, while desktop launches inject the selected program name into `Vm::with_modules_shared`.
- `WriteFile` writes to the host's stdout and ignores most handle semantics.
- The Win32 mode uses a flat process-level address space and a shared host-side GUI state, not Windows segmentation, native desktop window management, or per-process isolation. Exceptions follow Win32 SEH: software exceptions (`RaiseException`, C++ `throw`) and CPU faults (access violations, divide by zero) are dispatched through the guest's `FS:[0]` handler chain (`src/kernel/eh.rs`). The virtual desktop supports top-level geometry, z-order, dragging, resizing, activation/focus, and close-box input, but not minimize/maximize, child-window painting, or real desktop window management.
- Programs run side by side but cooperatively on one host thread: a program that computes for a long time without reading messages holds up the others. There is no guest-visible `CreateProcess`/`ShellExecute` yet (`ShellExecute` logs the request and reports success), and cross-program `SendMessage`/broadcasts do not reach other programs. `.COM` files are recognized but unsupported because the backend executes 32-bit PE images.
- The FAT32 layer is read-only and host-side. It has no write path, no host kernel mount, and no guest `CreateFileA`/`ReadFile`/`FindFirstFileA` implementation; Explorer reads image bytes directly for its display only.
- Genuine `PBRUSH.EXE` runs only far enough to call its launcher imports (5 API calls) and exit. Genuine `MSPAINT.EXE` runs interactively. Not yet implemented: the font and print dialogs, OLE embedding/linking and OLE Automation (OLEAUT32 conversions, MFCUIA32), threads, and caption icons from icon resources.
- Winsock uses host networking; there is no guest TCP/IP stack, NDIS/VxD, Winsock provider enumeration, or genuine `WSOCK32.DLL` execution. Async notifications are posted into the shared GUI message queue, but the guest only observes state recorded by the Rust layer. IE/WININET import traces are still pending because the RTM floppy set contains no IE binaries.
- Internet Explorer is not implemented. IE 3.0 is the first target; genuine `IEXPLORE.EXE`, `WININET.DLL`, `URLMON.DLL`, and `MSHTML.DLL` must be supplied or approved before that work can be traced and validated.
- The rendered guest window is a software view driven by guest edit state; native Win95 GDI painting, controls, menu commands, and common dialogs are not reproduced visually.
- The boot machine implements only `INT 10h` mode set `03h`/`13h` and teletype; there is no PIC, keyboard, disk BIOS, or BIOS data area yet.
- Only mode 13h is supported for boot graphics; planar EGA/VGA modes and VESA are not implemented.
- DAC writes are treated as 8-bit RGB values rather than VGA's 6-bit values; the default palette is identity/grayscale.
- The kernel mode still requires a flat 32-bit binary loaded by the host; it does not chain from the boot sector.
- VGA text rendering ignores attribute bytes. Boot graphics can be shown in a static host window; keyboard and pointer input for guest windows exists only in the process-level `display` runtime, not in the boot machine.

## Next milestones

1. Finish the network path for IE 3.0: obtain genuine IE binaries, trace `WININET.DLL`/`URLMON.DLL`, and extend Winsock and the message/UI stubs only where the trace requires it.
2. Broaden Winsock conformance with real guests: provider enumeration, more complete async name services, timeout/error edge cases, and additional IAT round-trip guests.
3. Broaden Paint coverage: ChooseFont/PrintDlg, and icon resources.
4. Broaden Notepad coverage: menu commands, Open/Save/Find dialogs, word wrap, and save-changes prompts, validating each against the Notepad disassembly (kept locally, not published).
5. Replace compatibility stubs with fuller KERNEL32 file/locale behavior, GDI32 drawing, COMDLG32 dialogs, and SHELL32 integration.
6. Move the software client renderer toward native Win95 metrics and painting so the host view more closely matches the original bitmap resources and font usage.
7. Grow the real-mode/protected-mode machine: PIC/IDT, keyboard, disk interface, richer VGA modes, and a handoff from the boot sector into a 32-bit kernel.
8. Add automated GUI smoke tests for the genuine Notepad open/move/resize/activate/type/close path and for desktop PE launching, then connect the read-only FAT32 reader to guest Win32 file/search APIs.

## Static-analysis inputs

- `docs/win95-notepad-analysis.md` — artifact identity, imports, resources.
- `docs/win95-api-dll-spec.md` — DLL exports/dependencies of the seven extracted Win95 DLLs, plus the Winsock artifacts.
- A Notepad disassembly and decompilation report (function map, message/command tables, file, find and print behavior) is kept locally and not published, since it reproduces Microsoft code.
