# Windows 95 API modules — implementation-side spec

Status: **the five DLLs imported directly by Notepad, plus their two transitive dependencies, are extracted, hashed and inventoried**. All report version `4.00.950` (RTM), matching Notepad.

## Modules

All files are in `binaries/win95/dlls/`, extracted from the same Win95 RTM English floppy set as `NOTEPAD.EXE`.

| Module | SHA-256 (first 16) | Size | Exports | Ordinal base | Direct PE dependencies |
|---|---|---:|---:|---:|---|
| `KERNEL32.DLL` | `fb1a1fd3964711a…` | 411,136 | 780 | 1 | **none** (no import directory) |
| `USER32.DLL` | `3ae9886657bc4ab0…` | 44,544 | 580 | 1 | KERNEL32 (81), GDI32 (5) |
| `GDI32.DLL` | `94047c1c14c974ce…` | 131,072 | 335 | 100 | KERNEL32 (108), ADVAPI32 (11) |
| `COMDLG32.DLL` | `3fc2341d0aa51d9c…` | 92,672 | 20 | 1 | KERNEL32 (54), USER32 (85), GDI32 (20), ADVAPI32 (4) |
| `SHELL32.DLL` | `46caa1f6abc00969…` | 817,664 | 252 | 2 | USER32 (189), KERNEL32 (165), GDI32 (45), COMCTL32 (68), ADVAPI32 (12) |
| `ADVAPI32.DLL` | `3f5fa2a030f5c6d4…` | 13,824 | 200 | 1 | KERNEL32 (16) |
| `COMCTL32.DLL` | `5bd0730aa4ad8c99…` | 182,272 | 104 | 2 | KERNEL32 (58), GDI32 (46), USER32 (142), ADVAPI32 (6) |

## Winsock artifacts

The network phase uses genuine RTM Winsock files as reverse-engineering references in `binaries/win95/net/`. A `SHA256SUMS` file beside them verifies all four artifacts. They are **not mapped into the guest**: `WSOCK32.DLL` and `WINSOCK.DLL` are deliberately replaced by the Rust WinSock layer in `src/win32/winsock.rs` so guest sockets are backed by host `std::net`.

| Artifact | SHA-256 |
|---|---|
| `wsock32.dll` | `2dbc73f5512d5a293411b70db18cc7d4ab0c21637fb905eaf38741413550b78e` |
| `winsock.dll` | `e7dddad7df6c82b9b9f54311a55095a96a783768d7d29457bb5c5a93e21e0fc2` |
| `wsock.vxd` | `e45de95511b8cc3505f35e69735072907f74b82ee57ee946a7bfd1f61a862043` |
| `inetmib1.dll` | `6a9a9adb46a4d8904a8434fe416bcaf85b6f561fd8788070d0a39d79c66e8155` |

The Rust layer currently covers the core WinSock 1.1 surface traced from these modules and the hand-built Winsock guest. Provider-enumeration and name-service-provider exports such as `EnumProtocolsA`, `GetAddressByNameA`, and `NPLoadNameSpaces` are inventoried here but not implemented; they will be added only if genuine IE/WININET imports require them.

## Import/export cross-check

Every one of Notepad's **127 imports resolves by name** to a genuine export in the corresponding Win95 RTM DLL. There are **no forwarded exports** among them and no ordinal-only imports. A name-based IAT resolver over these seven modules is therefore sufficient for this binary.

Key ordinals (useful if you later support ordinal imports):

- `COMDLG32`: CommDlgExtendedError=5, FindTextA=6, GetFileTitleA=8, GetOpenFileNameA=10, GetSaveFileNameA=12, PageSetupDlgA=14
- `USER32`: GetMessageA=266, CreateWindowExA=84, RegisterClassExA=424, DefWindowProcA=126, DispatchMessageA=140, TranslateMessage=547, MessageBoxA=387, wsprintfA=577, WinHelpA=569
- `KERNEL32`: CreateFileA=158, GetLastError=340, GetProfileStringA=386, GlobalAlloc=436, LocalAlloc=501, ExitProcess=214, `_lopen`=759, `_lread`=760, `_lwrite`=761, lstrlenA=779
- `GDI32`: CreateDCA=137, CreateFontA=148, StartDocA=415, EndDoc=182, SetAbortProc=370
- `SHELL32`: DragAcceptFiles=41, DragQueryFileA=50, ShellAboutA=243, ShellExecuteA=245

Full per-module ordinal maps were extracted and are retained in the REA evidence below.

## Dependency graph

```
              ADVAPI32 ─────┐
              (200 exp)     │
                   ▲        │
     KERNEL32 ◄────┼────────┼─────────────┐
     (780 exp)     │        │             │
        ▲   ▲      │        │             │
        │   └── GDI32 ◄─────┤             │
        │       (335 exp)   │             │
        │          ▲        │             │
        ├──── USER32 ───────┤             │
        │       (580 exp)   │             │
        │          ▲        │             │
        │      COMDLG32     │             │
        │       (20 exp)    │             │
        │                   ▼             │
        └──────────────── COMCTL32 ◄──────┘
                          (104 exp)
                             ▲
                          SHELL32
                          (252 exp)
```

`KERNEL32` has **no PE import directory** in this build — it is the effective base layer (its 16-bit thunks/VxD calls are outside the PE import graph). This matters: implementing KERNEL32 is unavoidable, and nothing below it can be delegated to another PE module.

## What the Rust implementation needs

Layered roughly bottom-up:

1. **Core / KERNEL32-compatible:** PE loading and IAT patching; local and global heaps; ANSI string helpers; the legacy `_l*` file API and `CreateFileA`; find-first/close; `GetProfileStringA`; locale/date/time formatting; process startup/exit.
2. **GDI32-compatible:** device contexts for screen and printer, stock objects, font creation and text metrics, mapping modes, and the document/abort print sequence.
3. **USER32-compatible:** window classes, `CreateWindowExA`, the edit control, menu/accelerator handling, the message loop, dialog procedure dispatch, clipboard, metrics, Beep/MessageBox, and help dispatch.
4. **ADVAPI32-compatible:** only the small surface the above DLLs actually call (registry/security helpers) — inventory its 16 KERNEL32 imports to scope it.
5. **COMDLG32-compatible:** Open/Save As, Find/Replace, Page Setup — these are the largest UI-heavy pieces.
6. **COMCTL32-compatible:** the subset SHELL32 uses (68 imports).
7. **SHELL32-compatible:** `ShellAboutA`, drag-and-drop, `ShellExecuteA` for the WordPad/help fallback.

Because Notepad imports by name and no forwarders are involved, the loader can start with a simple `name -> fn pointer` table per module. Ordinal support can wait.

## Evidence

- Export inventories: `ev_c7b59a645fd5fea58337f245ca995eca1e6c0a041a32177f827ee5544e3f1d7e` (kernel32), `ev_3e0491102072f8cb9d38e6d8a702dab0cdecfefecab8202e82f277d34bd70ee4` (user32), `ev_c1dfb80349a1b451185a8541b6e990e30f1adfcb039ec24542e8487f46ac05cb` (gdi32), `ev_8b688105a35ace1560acdd0260e46dc911437915d6b359c0896c72dceb491e83` (comdlg32), `ev_dada076ee174fc7311a39e850c2c573a0b2540a54c3e5082f65ff29a269c3ede` (shell32), `ev_be4315b94456539b84e637ef3da2144ba173fc9b3acb23c5b666174422c7aec0` (advapi32), `ev_72f20814758cae7314299179a8dbe891a7dc40dc094396664459a94b20c475f0` (comctl32).
- Complete export row records (for name/ordinal/address paging): `ev_7d724fd1481c3131e12504bc8ad2797d06715872025768ae6665ad9410e1bbf7` (kernel32), `ev_fe3faff566787115f9abdd90fa3b7b0992930391828b5f368464e5e02b1a46c9` (user32), `ev_65cc5bff18c562cff61300c64b9dd94d542b5dd6385b07e0f09763adf2c3d158` (gdi32), `ev_9a4d6dc0e875ff241595322e12085ae3fe510a6005dc3f1a75aad20137dd6d1a` (comdlg32), `ev_ebe27cef76e387a6bf8704f5ad922849c84b818f6bb95cf82d2427c8f99fad2a` (shell32).
- DLL import graphs: `ev_bfd5a6611b4531c5369d958e4208831c2a4be94561d8c4830091a87277a8decc` (kernel32), `ev_7cad337187010989cfc4ca98442a91469c94d208f888847e2fcf3c5516dec19c` (user32), `ev_00d5d0befdbee8bdfa4f9b4b1450a18d4e1d9861477458c22a0e6f087a44dd93` (gdi32), `ev_b34aa4ac4ed6384affb79212b2e6a8aa5bb8a4ccb279f0d515f0f7fd7eda53da` (comdlg32), `ev_1e9e4a648e037755e37d04f910d25ec8944a30eeab620554598729ed76bcb24c` (shell32), `ev_8ed958ed80e7aac24ad07086fdc21ad6c9a08ae0b98e13219e30b06d27d99778` (advapi32), `ev_3f920634ad786e1e4eaed7432480cd19bccd7b3ff6ec35fbda6f62f6978659b1` (comctl32).

## Limitations / unknowns

- Export and import directories describe declarations, not reachable code or runtime binding. No DLL was executed.
- `KERNEL32`'s missing import directory does **not** prove it has no external dependencies; Win95 kernel thunks and VxD calls are outside the PE import table and were not analyzed.
- The export counts for `SHELL32` (252) and `COMCTL32` (104) come from export directories whose address tables contain more entries than named exports; ordinal-only exports have no name and are not individually listed here.
- This spec covers the *static module surface*. Semantics — message numbers, structure layouts, dialog behavior, and calling conventions — still require disassembly of the relevant functions and/or the original SDK headers.
