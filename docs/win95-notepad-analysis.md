# Windows 95 Notepad — static binary analysis

Status: **artifact acquired and identified; API surface extracted; genuine binary executed in the OTW VM**. This document is the static artifact/API analysis; runtime behavior is reported in `docs/win95-vm-architecture.md`.

## Artifact identity

| Field | Value |
|---|---|
| Path | `binaries/win95/NOTEPAD.EXE` |
| SHA-256 | `0c90f92bedb3e437d0d9339219cb1f2898f44ed8172d88cf4bb62d29e9796f06` |
| Size | 34,304 bytes |
| Format | PE32, machine 0x014c (i386), 6 sections, GUI subsystem 4.00 |
| Version resource | `4.00.950`, "Windows Notepad application file", © Microsoft Corp. 1991–1995 |
| Language | 1033 (en-US) |
| PE import hash (pefile) | `dd0b45a0c8731a952fbe65b3aa913d07` |

**Provenance:** extracted from `WIN95_04.CAB` on disk 4 of the WinWorld *Windows 95 RTM (English, 3.5-inch)* set. The downloaded 7z archive matched WinWorld's published SHA-512 `54664aad…7d888` exactly. `NOTEPAD.HLP` and `NOTEPAD.CNT` came from `WIN95_09.CAB`. The extracted file's SHA-256 matches the independently published reference hash for Win95 RTM Notepad.

## Runtime milestone

The unmodified `NOTEPAD.EXE` is now executed by the OTW process-level Win32 VM. With the optional `display` feature it opens an `Untitled - Notepad` host window, completes its WinMain/init path, enters its real `GetMessageA`/`DispatchMessageA` loop, accepts keyboard input in the guest edit control, and exits with status 0 when the host close event drives its WM_CLOSE/WM_DESTROY path. The Rust API layer implements all 127 imports listed below; several advanced dialog, print, and shell entries are compatibility stubs rather than full Win95 implementations.

## Minimum API surface (observed imports)

127 named imports across 5 DLLs; **no delay-load imports**. This is the contract the Rust compatibility layer must satisfy for this binary to load.

### SHELL32.dll — 5
`ShellExecuteA`, `DragQueryFileA`, `DragFinish`, `DragAcceptFiles`, `ShellAboutA`

### KERNEL32.dll — 35
`LocalAlloc`, `LocalLock`, `LocalReAlloc`, `LocalUnlock`, `LocalFree`,
`GlobalAlloc`, `GlobalLock`, `GlobalUnlock`, `GlobalFree`,
`lstrlenA`, `lstrcmpA`, `lstrcmpiA`, `lstrcpyA`, `lstrcpynA`, `lstrcatA`,
`_lopen`, `_lread`, `_lwrite`, `_llseek`, `_lclose`, `DeleteFileA`, `CreateFileA`,
`GetLastError`, `FindFirstFileA`, `FindClose`, `GetProfileStringA`,
`GetCommandLineA`, `GetStartupInfoA`, `GetModuleHandleA`, `ExitProcess`,
`GetLocalTime`, `GetTimeFormatA`, `GetDateFormatA`, `GetLocaleInfoA`

### USER32.dll — 60
`RegisterClassExA`, `CreateWindowExA`, `DefWindowProcA`, `GetMessageA`, `PeekMessageA`,
`TranslateMessage`, `DispatchMessageA`, `TranslateAcceleratorA`, `IsDialogMessageA`,
`LoadAcceleratorsA`, `LoadIconA`, `LoadCursorA`, `LoadStringA`, `RegisterWindowMessageA`,
`SetWindowLongA`, `GetSystemMetrics`, `GetSystemMenu`, `GetMenu`, `GetSubMenu`,
`CheckMenuItem`, `EnableMenuItem`, `GetWindowTextA`, `SetWindowTextA`, `GetDlgItemTextA`,
`SetDlgItemTextA`, `GetDlgCtrlID`, `SendDlgItemMessageA`, `CreateDialogParamA`, `EndDialog`,
`SendMessageA`, `PostMessageA`, `PostQuitMessage`, `ShowWindow`, `UpdateWindow`,
`DestroyWindow`, `IsIconic`, `SetForegroundWindow`, `GetLastActivePopup`, `MoveWindow`,
`InvalidateRect`, `GetClientRect`, `GetFocus`, `SetFocus`, `ChildWindowFromPoint`,
`ScreenToClient`, `GetCursorPos`, `GetDC`, `ReleaseDC`, `SetCursor`, `MessageBoxA`,
`MessageBeep`, `EnableWindow`, `wsprintfA`, `CharNextA`, `CharPrevA`, `OpenClipboard`,
`CloseClipboard`, `IsClipboardFormatAvailable`, `WinHelpA`

### GDI32.dll — 21
`CreateDCA`, `DeleteDC`, `GetDeviceCaps`, `GetStockObject`, `SelectObject`, `DeleteObject`,
`CreateFontA`, `GetTextMetricsA`, `GetTextExtentPointA`, `GetTextCharset`, `SetBkMode`,
`SetMapMode`, `SetWindowExtEx`, `SetViewportExtEx`, `LPtoDP`, `TabbedTextOutA`,
`StartDocA`, `StartPage`, `EndPage`, `EndDoc`, `AbortDoc`, `SetAbortProc`

### comdlg32.dll — 6
`GetOpenFileNameA`, `GetSaveFileNameA`, `FindTextA`, `PageSetupDlgA`, `GetFileTitleA`, `CommDlgExtendedError`

## Resources

16 resources, all en-US:

| RT id | Meaning | Instances |
|---|---|---|
| 3 | RT_ICON | 4 (16×16 and 32×32 variants) |
| 4 | RT_MENU | 1 (600-byte menu template; 4 popups, 19 commands, 6 separators) |
| 5 | RT_DIALOG | 2 (dialog #12 = find/replace helper, #14 = page setup) |
| 6 | RT_STRING | 4 blocks (menus, labels, errors; IDs 1–57 decoded) |
| 9 | RT_ACCELERATOR | 2 |
| 14 | RT_GROUP_ICON | 2 |
| 16 | RT_VERSION | 1 |

## UI contract from resource strings

Menu structure decoded from the 600-byte `RT_MENU` payload at file offset `23592` (RVA `0x8c18`, SHA-256 `5b17153e2d55569a3ba344265ac940f67b6891aaf2622041cea18ce8b2c892c7`). 32-bit template items are `WORD flags; WORD id; WCHAR text[]`; popups omit `id`. Flags: `MF_POPUP=0x10`, `MF_END=0x80`, `MF_GRAYED=0x01`; Cut/Copy/Paste/Delete start grayed, the rest enabled.

| Popup | Item | ID |
|---|---|---|
| `&File` | `&New` | `9` |
| `&File` | `&Open...` | `10` |
| `&File` | `&Save` | `1` |
| `&File` | `Save &As...` | `2` |
| `&File` | `Page Se&tup...` | `0x20` |
| `&File` | `&Print` | `0x0e` |
| `&File` | `E&xit` | `0x1c` |
| `&Edit` | `&Undo\tCtrl+Z` | `0x19` |
| `&Edit` | `Cu&t\tCtrl+X` | `0x300` |
| `&Edit` | `&Copy\tCtrl+C` | `0x301` |
| `&Edit` | `&Paste\tCtrl+V` | `0x302` |
| `&Edit` | `De&lete\tDel` | `0x303` |
| `&Edit` | `Select &All` | `7` |
| `&Edit` | `Time/&Date\tF5` | `0x0c` |
| `&Edit` | `&Word Wrap` | `0x1b` |
| `&Search` | `&Find...` | `3` |
| `&Search` | `Find &Next\tF3` | `8` |
| `&Help` | `&Help Topics` | `5` |
| `&Help` | `&About Notepad` | `0x0b` |

Six further entries are empty-text separators (id `0`). Accelerator table 1 maps `F1`→`5`, `F3`→`8`, `F5`→`0x0c`, `Ctrl+Z`/`Ctrl+Backspace`→`0x19`, `Ctrl+X`→`0x300`, `Ctrl+C`→`0x301`, `Ctrl+V`→`0x302`, `Alt+-`→`0x301`. The `/SETUP` variant (table 2) additionally maps `Esc`, `Alt+C`, `Alt+D`, `Alt+Shift+Z`→`0x1c`.

Other observed strings: inline `Notepad`, `windows`, `device`, `DEV Error!`, `Out of RC string space!!`, `commdlg_FindReplace`, date/time format templates `%2.2d\%2.2d\%2.2d` and `%2.2d:%2.2d:%4.4d`; RT_STRING ids 1–57 supply `Untitled`, ` - Notepad`, `\*.txt`, `Text Documents`, `All Files (*.*)`, `notepad.hlp`, `wordpad.exe`, `/p `, page-header tokens `fFpPtTdDcCrRlL`, and the standard error messages (file not found, too large, no permission, save changes?, overwrite?, out of memory, clipboard line too long, printer errors). The full string table and command handlers are in a Ghidra disassembly and decompilation report kept out of the repository because it reproduces Microsoft code.

## What this implies for the Rust implementation

1. **Loader:** PE32 with 6 sections, no managed runtime, no 16-bit WOW dependency implied by the import table. Need an IAT resolver for five DLLs and x86 `stdcall` call convention.
2. **Window system:** a `Notepad` window class, a standard multiline *edit* child control, menu bar, accelerators, system menu, drag-and-drop target, clipboard, and a `GetMessage`/`TranslateMessage`/`DispatchMessage` loop.
3. **Kernel:** local and global heaps, ANSI string helpers, legacy `_l*` file I/O plus `CreateFileA`, profile-string config, locale-aware date/time formatting.
4. **Common dialogs:** Open, Save As, Find/Replace, Page Setup — these are large sub-surfaces with their own message/control behavior.
5. **Printing:** a complete GDI print path (StartDoc/StartPage/EndPage/EndDoc plus text metrics) and an abort procedure.
6. **Shell integration:** `ShellAboutA` (About box), `DragAcceptFiles`/`DragQueryFileA`/`DragFinish`, `ShellExecuteA` for the WordPad fallback and help.

## Evidence and limitations

- `inspect_pe_imports` evidence: `ev_f93f71dc36bf9883cae16a124d9b1ef58e5217b9572a2750f8f112f689348bc9` (coverage complete, 5 DLLs / 127 functions).
- `inspect_pe_resources` evidence: `ev_9e0fc6f6c339e98e96da40c68fb2671261b32510802cbb6f6ac911203e7218f8` (coverage complete, 16 resources).
- This section is static analysis. Imports prove declarations in the PE, not that every function is reachable at runtime; bound-import snapshots and loaded IAT contents were not observed. The RT_MENU and RT_STRING payloads have been decoded (in a Ghidra disassembly and decompilation report kept out of the repository because it reproduces Microsoft code); dialog-template semantics are still inferred from byte payloads and associated strings. Runtime coverage to date is the startup/message-loop/edit-input/close path described above.
- No dependent Win95 system DLLs have been analyzed yet, so this spec describes what Notepad *calls*, not everything a host must implement to satisfy those calls.
