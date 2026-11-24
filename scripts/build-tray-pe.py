#!/usr/bin/env python3
"""Build TRAYTEST.EXE: a Win32 guest that puts an icon in the taskbar
notification area.

It registers a window class, creates a hidden window, and adds a tray icon
(`Shell_NotifyIconA`) with the system application icon and the tip
"OTW tray test". A double-click on the icon changes the tip to "Clicked";
a right-click removes the icon and quits with exit code 0.
"""

from pathlib import Path
import struct
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "guests" / "TRAYTEST.EXE"
IMAGE_BASE = 0x400000
TEXT_VA = 0x401000

ASM = r"""
.intel_syntax noprefix
.set BASE, 0x400000
.set WM_TRAY, 0x401
.text
.globl start
start:
    push 0
    call dword ptr [iat_GetModuleHandleA]
    mov [wc_instance], eax
    push offset wndclass
    call dword ptr [iat_RegisterClassA]
    push 0                          # lpParam
    push dword ptr [wc_instance]    # hInstance
    push 0                          # hMenu
    push 0                          # hWndParent
    push 0                          # height
    push 0                          # width
    push 0                          # y
    push 0                          # x
    push 0                          # style: hidden
    push offset title
    push offset class_name
    push 0                          # dwExStyle
    call dword ptr [iat_CreateWindowExA]
    mov [nid_hwnd], eax
    push 32512                      # IDI_APPLICATION
    push 0
    call dword ptr [iat_LoadIconA]
    mov [nid_icon], eax
    push offset nid
    push 0                          # NIM_ADD
    call dword ptr [iat_Shell_NotifyIconA]
message_loop:
    push 0
    push 0
    push 0
    push offset msg
    call dword ptr [iat_GetMessageA]
    test eax, eax
    jz done
    push offset msg
    call dword ptr [iat_TranslateMessage]
    push offset msg
    call dword ptr [iat_DispatchMessageA]
    jmp message_loop
done:
    push 0
    call dword ptr [iat_ExitProcess]

# LRESULT WndProc(hwnd, msg, wparam, lparam)
wndproc:
    cmp dword ptr [esp+8], WM_TRAY
    jne default
    mov eax, [esp+16]
    cmp eax, 0x203                  # WM_LBUTTONDBLCLK
    je double_click
    cmp eax, 0x205                  # WM_RBUTTONUP
    je right_click
    xor eax, eax
    ret 16
double_click:
    mov esi, offset clicked
    mov edi, offset nid_tip
    mov ecx, 8
    rep movsb
    mov dword ptr [nid_flags], 4    # NIF_TIP
    push offset nid
    push 1                          # NIM_MODIFY
    call dword ptr [iat_Shell_NotifyIconA]
    xor eax, eax
    ret 16
right_click:
    push offset nid
    push 2                          # NIM_DELETE
    call dword ptr [iat_Shell_NotifyIconA]
    push 0
    call dword ptr [iat_PostQuitMessage]
    xor eax, eax
    ret 16
default:
    jmp dword ptr [iat_DefWindowProcA]

.data
.align 4
wndclass:
    .long 0, wndproc, 0, 0
wc_instance: .long 0
    .long 0, 0, 0, 0, class_name
msg: .fill 28, 1, 0
# NOTIFYICONDATAA
nid: .long 88
nid_hwnd: .long 0
    .long 1                         # uID
nid_flags: .long 7                  # NIF_MESSAGE | NIF_ICON | NIF_TIP
    .long WM_TRAY
nid_icon: .long 0
nid_tip: .asciz "OTW tray test"
    .fill 64 - 14, 1, 0
class_name: .asciz "OTWTray"
title: .asciz "Tray test"
clicked: .asciz "Clicked"

# ---- imports ----
.align 4
.globl idesc
idesc:
    .long kernel_int - BASE, 0, 0, kernel_name - BASE, kernel_iat - BASE
    .long user_int - BASE, 0, 0, user_name - BASE, user_iat - BASE
    .long shell_int - BASE, 0, 0, shell_name - BASE, shell_iat - BASE
    .long 0, 0, 0, 0, 0
kernel_int: .long hn_module - BASE, hn_exit - BASE, 0
kernel_iat:
iat_GetModuleHandleA: .long hn_module - BASE
iat_ExitProcess: .long hn_exit - BASE
    .long 0
user_int: .long hn_register - BASE, hn_create - BASE, hn_icon - BASE, hn_get - BASE, hn_translate - BASE, hn_dispatch - BASE, hn_default - BASE, hn_quit - BASE, 0
user_iat:
iat_RegisterClassA: .long hn_register - BASE
iat_CreateWindowExA: .long hn_create - BASE
iat_LoadIconA: .long hn_icon - BASE
iat_GetMessageA: .long hn_get - BASE
iat_TranslateMessage: .long hn_translate - BASE
iat_DispatchMessageA: .long hn_dispatch - BASE
iat_DefWindowProcA: .long hn_default - BASE
iat_PostQuitMessage: .long hn_quit - BASE
    .long 0
shell_int: .long hn_notify - BASE, 0
shell_iat:
iat_Shell_NotifyIconA: .long hn_notify - BASE
    .long 0
.align 2
hn_module: .short 0
    .asciz "GetModuleHandleA"
.align 2
hn_exit: .short 0
    .asciz "ExitProcess"
.align 2
hn_register: .short 0
    .asciz "RegisterClassA"
.align 2
hn_create: .short 0
    .asciz "CreateWindowExA"
.align 2
hn_icon: .short 0
    .asciz "LoadIconA"
.align 2
hn_get: .short 0
    .asciz "GetMessageA"
.align 2
hn_translate: .short 0
    .asciz "TranslateMessage"
.align 2
hn_dispatch: .short 0
    .asciz "DispatchMessageA"
.align 2
hn_default: .short 0
    .asciz "DefWindowProcA"
.align 2
hn_quit: .short 0
    .asciz "PostQuitMessage"
.align 2
hn_notify: .short 0
    .asciz "Shell_NotifyIconA"
kernel_name: .asciz "KERNEL32.dll"
user_name: .asciz "USER32.dll"
shell_name: .asciz "SHELL32.dll"
.globl image_end
image_end:
"""


def assemble():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / "tray.s").write_text(ASM)
        subprocess.run(["as", "--32", "-o", tmp / "tray.o", tmp / "tray.s"], check=True)
        script = tmp / "link.ld"
        script.write_text(
            "SECTIONS { . = 0x%x; .text : { *(.text) *(.data) } /DISCARD/ : { *(*) } }\n" % TEXT_VA
        )
        subprocess.run(["ld", "-m", "elf_i386", "-T", script, "-e", "start", "-o", tmp / "tray.elf", tmp / "tray.o"], check=True)
        subprocess.run(["objcopy", "-O", "binary", "-j", ".text", tmp / "tray.elf", tmp / "tray.bin"], check=True)
        symbols = {}
        for line in subprocess.run(["nm", tmp / "tray.elf"], capture_output=True, text=True, check=True).stdout.splitlines():
            parts = line.split()
            if len(parts) == 3:
                symbols[parts[2]] = int(parts[0], 16)
        return (tmp / "tray.bin").read_bytes(), symbols


def build():
    code, symbols = assemble()
    raw_size = (len(code) + 0x1FF) & ~0x1FF
    virtual_size = (len(code) + 0xFFF) & ~0xFFF
    size_of_image = 0x1000 + virtual_size
    pe_offset = 0x80
    headers = bytearray(0x200)
    headers[0:2] = b"MZ"
    struct.pack_into("<I", headers, 0x3C, pe_offset)
    struct.pack_into("<4sHHIIIHH", headers, pe_offset, b"PE\0\0", 0x14C, 1, 0, 0, 0, 0xE0, 0x0103)
    opt = pe_offset + 24
    struct.pack_into(
        "<HBBIIIIIIIIIHHHHHHIIIIHHIIIIII",
        headers,
        opt,
        0x10B, 1, 0,
        raw_size, 0, 0,
        symbols["start"] - IMAGE_BASE,
        0x1000, 0x1000,
        IMAGE_BASE, 0x1000, 0x200,
        4, 0, 0, 0, 4, 0,
        0,
        size_of_image, 0x200, 0,
        2, 0,
        0x100000, 0x1000, 0x100000, 0x1000, 0, 16,
    )
    directories = opt + 96
    struct.pack_into("<II", headers, directories + 8, symbols["idesc"] - IMAGE_BASE, 40)
    section = opt + 0xE0
    struct.pack_into("<8sIIIIIIHHI", headers, section, b".text", virtual_size, 0x1000, raw_size, 0x200, 0, 0, 0, 0, 0xE0000060)
    image = bytes(headers) + code + b"\0" * (raw_size - len(code))
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_bytes(image)
    print(f"wrote {OUTPUT} ({len(image)} bytes)")


if __name__ == "__main__":
    build()
