#!/usr/bin/env python3
"""Build EHTEST.EXE: a Win32 guest exercising MSVC exception handling.

The machine code is assembled with GNU as (32-bit) and wrapped in a PE by
hand. The exception tables use the MSVC 2.x-4.x layout (FuncInfo magic
0x19930520) that Win95-era programs and MFC 3.0 were compiled with, and the
guest imports `_CxxThrowException`, `__CxxFrameHandler` and
`_except_handler3` from MSVCRT20.dll, exactly like compiled code.

Checks, each setting a result the final block verifies:
  1. catch (int) skips a non-matching catch (double) and runs the
     destructor of an object constructed inside the try block;
  2. a throw through an intermediate frame without a matching catch runs that
     frame's destructor (global unwind) and is caught by catch (...);
  3. a rethrow (`throw;`) from a catch block reaches an outer catch (int);
  4. __try/__except catches RaiseException, with a filter reading the code;
  5. __finally runs while __except unwinds an inner level;
  6. a filter returning EXCEPTION_CONTINUE_EXECUTION resumes after the raise;
  7. a read from unmapped memory raises an access violation whose record
     names the faulting instruction, read access and address;
  8. a filter repairs a faulting write through CONTEXT.Eax and continues,
     so the instruction is retried successfully;
  9. an integer divide by zero raises STATUS_INTEGER_DIVIDE_BY_ZERO;
 10. C++ catch (...) catches an access violation from a called function and
     the intermediate frame's destructor runs.
It prints "EH OK" and exits with 0, or exits with a bitmask of failures.
"""

from pathlib import Path
import struct
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "guests" / "EHTEST.EXE"
IMAGE_BASE = 0x400000
TEXT_VA = 0x401000

ASM = r"""
.intel_syntax noprefix
.set BASE, 0x400000
.text
.globl start
start:
    call test_catch_int
    call test_global_unwind
    call test_rethrow_outer
    mov eax, [dtor_count]
    mov [dtor_count_early], eax
    call test_seh
    call test_continue
    call test_av_read
    call test_av_continue
    call test_divide
    call test_cxx_av
    # Verify results; build a failure bitmask in ebx.
    xor ebx, ebx
    cmp dword ptr [result_int], 1042
    je 1f
    or ebx, 1
1:  cmp dword ptr [dtor_count_early], 2
    je 1f
    or ebx, 2
1:  cmp dword ptr [catchall_hit], 1
    je 1f
    or ebx, 4
1:  cmp dword ptr [rethrow_value], 7
    je 1f
    or ebx, 8
1:  cmp dword ptr [seh_result], 1
    je 1f
    or ebx, 16
1:  cmp dword ptr [finally_count], 1
    je 1f
    or ebx, 32
1:  cmp dword ptr [continue_result], 1
    je 1f
    or ebx, 64
1:  cmp dword ptr [av_result], 1
    je 1f
    or ebx, 128
1:  cmp dword ptr [fixed_target], 5
    je 1f
    or ebx, 256
1:  cmp dword ptr [div_result], 1
    je 1f
    or ebx, 512
1:  cmp dword ptr [cxx_av_hit], 1
    je 1f
    or ebx, 1024
1:  cmp dword ptr [dtor_count], 3
    je 1f
    or ebx, 2048
1:  test ebx, ebx
    jnz 2f
    push -11
    call dword ptr [iat_GetStdHandle]
    push 0
    push offset written
    push 6
    push offset ok_text
    push eax
    call dword ptr [iat_WriteFile]
2:  push ebx
    call dword ptr [iat_ExitProcess]

# ---- 1: catch (int) with a destructor inside the try ----
test_catch_int:
    push ebp
    mov ebp, esp
    push -1
    push offset stub_catch_int
    push dword ptr fs:[0]
    mov fs:[0], esp
    sub esp, 16
    mov [ebp-0x10], esp
    mov dword ptr [ebp-4], 0          # enter try
    mov dword ptr [ebp-4], 1          # object constructed inside the try
    mov dword ptr [thrown_int], 42
    push offset throwinfo_int
    push offset thrown_int
    call dword ptr [iat__CxxThrowException]
    int3
cont_catch_int:
    mov eax, [ebp-0x18]
    mov [result_int], eax
    mov ecx, [ebp-0x0c]
    mov fs:[0], ecx
    mov esp, ebp
    pop ebp
    ret
stub_catch_int:
    mov eax, offset funcinfo_catch_int
    jmp dword ptr [iat___CxxFrameHandler]
funclet_catch_int:
    add dword ptr [ebp-0x18], 1000
    mov eax, offset cont_catch_int
    ret
funclet_catch_double:
    mov dword ptr [result_int], 999
    mov eax, offset cont_catch_int
    ret
funclet_dtor:
    inc dword ptr [dtor_count]
    ret

# ---- 2: global unwind through a frame without a matching catch ----
test_global_unwind:
    push ebp
    mov ebp, esp
    push -1
    push offset stub_outer
    push dword ptr fs:[0]
    mov fs:[0], esp
    sub esp, 16
    mov [ebp-0x10], esp
    mov dword ptr [ebp-4], 0
    call inner_throw
    int3
cont_outer:
    mov ecx, [ebp-0x0c]
    mov fs:[0], ecx
    mov esp, ebp
    pop ebp
    ret
stub_outer:
    mov eax, offset funcinfo_outer
    jmp dword ptr [iat___CxxFrameHandler]
funclet_catch_all:
    mov dword ptr [catchall_hit], 1
    mov eax, offset cont_outer
    ret
inner_throw:
    push ebp
    mov ebp, esp
    push -1
    push offset stub_inner
    push dword ptr fs:[0]
    mov fs:[0], esp
    sub esp, 16
    mov [ebp-0x10], esp
    mov dword ptr [ebp-4], 0          # object alive; no try here
    mov dword ptr [thrown_int], 5
    push offset throwinfo_int
    push offset thrown_int
    call dword ptr [iat__CxxThrowException]
    int3
stub_inner:
    mov eax, offset funcinfo_inner
    jmp dword ptr [iat___CxxFrameHandler]

# ---- 3: rethrow from a catch block ----
test_rethrow_outer:
    push ebp
    mov ebp, esp
    push -1
    push offset stub_rethrow_outer
    push dword ptr fs:[0]
    mov fs:[0], esp
    sub esp, 16
    mov [ebp-0x10], esp
    mov dword ptr [ebp-4], 0
    call rethrow_inner
    int3
cont_rethrow_outer:
    mov eax, [ebp-0x18]
    mov [rethrow_value], eax
    mov ecx, [ebp-0x0c]
    mov fs:[0], ecx
    mov esp, ebp
    pop ebp
    ret
stub_rethrow_outer:
    mov eax, offset funcinfo_rethrow_outer
    jmp dword ptr [iat___CxxFrameHandler]
funclet_rethrow_outer:
    mov eax, offset cont_rethrow_outer
    ret
rethrow_inner:
    push ebp
    mov ebp, esp
    push -1
    push offset stub_rethrow_inner
    push dword ptr fs:[0]
    mov fs:[0], esp
    sub esp, 16
    mov [ebp-0x10], esp
    mov dword ptr [ebp-4], 0
    mov dword ptr [thrown_int], 7
    push offset throwinfo_int
    push offset thrown_int
    call dword ptr [iat__CxxThrowException]
    int3
stub_rethrow_inner:
    mov eax, offset funcinfo_rethrow_inner
    jmp dword ptr [iat___CxxFrameHandler]
funclet_rethrow_inner:
    push 0
    push 0
    call dword ptr [iat__CxxThrowException]     # throw;
    int3

# ---- 4 + 5: __try/__except with an inner __finally ----
test_seh:
    push ebp
    mov ebp, esp
    push -1                           # [ebp-4]  trylevel
    push offset scopetable_seh        # [ebp-8]  scope table
    push dword ptr [iat__except_handler3]   # [ebp-0xc] handler
    push dword ptr fs:[0]             # [ebp-0x10] next
    mov fs:[0], esp
    sub esp, 8                        # [ebp-0x14] xpointers, [ebp-0x18] saved esp
    mov [ebp-0x18], esp
    mov dword ptr [ebp-4], 0          # outer __try
    mov dword ptr [ebp-4], 1          # inner __try/__finally
    push 0
    push 0
    push 0
    push 0xC0001234
    call dword ptr [iat_RaiseException]
    int3
seh_filter:
    mov eax, [ebp-0x14]               # EXCEPTION_POINTERS*
    mov eax, [eax]                    # EXCEPTION_RECORD*
    xor ecx, ecx
    cmp dword ptr [eax], 0xC0001234
    sete cl
    mov eax, ecx                      # EXECUTE_HANDLER if ours
    ret
seh_handler:
    mov esp, [ebp-0x18]
    mov dword ptr [seh_result], 1
    mov dword ptr [ebp-4], -1
    mov ecx, [ebp-0x10]
    mov fs:[0], ecx
    mov esp, ebp
    pop ebp
    ret
seh_finally:
    inc dword ptr [finally_count]
    ret

# ---- 6: EXCEPTION_CONTINUE_EXECUTION ----
test_continue:
    push ebp
    mov ebp, esp
    push -1
    push offset scopetable_continue
    push dword ptr [iat__except_handler3]
    push dword ptr fs:[0]
    mov fs:[0], esp
    sub esp, 8
    mov [ebp-0x18], esp
    mov dword ptr [ebp-4], 0
    push 0
    push 0
    push 0
    push 0xC0004321
    call dword ptr [iat_RaiseException]
    mov dword ptr [continue_result], 1   # reached only by continuing
    mov dword ptr [ebp-4], -1
    mov ecx, [ebp-0x10]
    mov fs:[0], ecx
    mov esp, ebp
    pop ebp
    ret
continue_filter:
    mov eax, -1                       # EXCEPTION_CONTINUE_EXECUTION
    ret
continue_handler:
    int3

# SEH frame prologue shared by tests 7-9: [ebp-4] trylevel, [ebp-8] scope
# table, [ebp-0xc] handler, [ebp-0x10] next, [ebp-0x14] xpointers,
# [ebp-0x18] saved esp.
.macro seh_enter table
    push ebp
    mov ebp, esp
    push -1
    push offset \table
    push dword ptr [iat__except_handler3]
    push dword ptr fs:[0]
    mov fs:[0], esp
    sub esp, 8
    mov [ebp-0x18], esp
    mov dword ptr [ebp-4], 0
.endm
.macro seh_leave
    mov dword ptr [ebp-4], -1
    mov ecx, [ebp-0x10]
    mov fs:[0], ecx
    mov esp, ebp
    pop ebp
    ret
.endm

# ---- 7: access violation on a read ----
test_av_read:
    seh_enter scopetable_av_read
    mov eax, 0x10
fault_read:
    mov ecx, [eax]
    int3
av_read_filter:
    mov eax, [ebp-0x14]
    mov eax, [eax]                    # EXCEPTION_RECORD*
    xor ecx, ecx
    cmp dword ptr [eax], 0xC0000005
    jne 1f
    cmp dword ptr [eax+12], offset fault_read
    jne 1f
    cmp dword ptr [eax+16], 2
    jne 1f
    cmp dword ptr [eax+20], 0         # read
    jne 1f
    cmp dword ptr [eax+24], 0x10      # address
    jne 1f
    inc ecx
1:  mov eax, ecx
    ret
av_read_handler:
    mov esp, [ebp-0x18]
    mov dword ptr [av_result], 1
    seh_leave

# ---- 8: repair a faulting write and continue ----
test_av_continue:
    seh_enter scopetable_av_continue
    mov eax, 0x20
    mov dword ptr [eax], 5            # faults, then retried with a valid eax
    seh_leave
av_continue_filter:
    mov eax, [ebp-0x14]
    mov ecx, [eax]                    # EXCEPTION_RECORD*
    mov edx, [eax+4]                  # CONTEXT*
    xor eax, eax
    cmp dword ptr [ecx+20], 1         # write
    jne 1f
    cmp dword ptr [ecx+24], 0x20
    jne 1f
    mov dword ptr [edx+0xB0], offset fixed_target
    mov eax, -1                       # EXCEPTION_CONTINUE_EXECUTION
1:  ret
av_continue_handler:
    int3

# ---- 9: integer divide by zero ----
test_divide:
    seh_enter scopetable_divide
    mov eax, 10
    xor edx, edx
    xor ecx, ecx
    div ecx
    int3
divide_filter:
    mov eax, [ebp-0x14]
    mov eax, [eax]
    xor ecx, ecx
    cmp dword ptr [eax], 0xC0000094
    sete cl
    mov eax, ecx
    ret
divide_handler:
    mov esp, [ebp-0x18]
    mov dword ptr [div_result], 1
    seh_leave

# ---- 10: C++ catch (...) of an access violation ----
test_cxx_av:
    push ebp
    mov ebp, esp
    push -1
    push offset stub_cxx_av
    push dword ptr fs:[0]
    mov fs:[0], esp
    sub esp, 16
    mov [ebp-0x10], esp
    mov dword ptr [ebp-4], 0
    call cxx_av_inner
    int3
cont_cxx_av:
    mov ecx, [ebp-0x0c]
    mov fs:[0], ecx
    mov esp, ebp
    pop ebp
    ret
stub_cxx_av:
    mov eax, offset funcinfo_outer_av
    jmp dword ptr [iat___CxxFrameHandler]
funclet_cxx_av:
    mov dword ptr [cxx_av_hit], 1
    mov eax, offset cont_cxx_av
    ret
cxx_av_inner:
    push ebp
    mov ebp, esp
    push -1
    push offset stub_inner
    push dword ptr fs:[0]
    mov fs:[0], esp
    sub esp, 16
    mov [ebp-0x10], esp
    mov dword ptr [ebp-4], 0          # object alive
    xor eax, eax
    mov dword ptr [eax+0x30], 1       # write fault
    int3

.data
.align 4
result_int: .long 0
dtor_count: .long 0
catchall_hit: .long 0
rethrow_value: .long 0
seh_result: .long 0
finally_count: .long 0
continue_result: .long 0
dtor_count_early: .long 0
av_result: .long 0
fixed_target: .long 0
div_result: .long 0
cxx_av_hit: .long 0
thrown_int: .long 0
written: .long 0
ok_text: .ascii "EH OK\n"

.align 4
# TypeDescriptors: vtable, spare, decorated name.
td_int: .long 0, 0
    .asciz ".H"
.align 4
td_double: .long 0, 0
    .asciz ".N"
.align 4
# CatchableType: flags, type, mdisp, pdisp, vdisp, size, copy ctor.
ct_int: .long 1, td_int, 0, -1, 0, 4, 0
ctt_int: .long 1, ct_int
# ThrowInfo: flags, destructor, forward-compat, catchable type table.
throwinfo_int: .long 0, 0, 0, ctt_int

# Test 1: state 0 = try, state 1 = object inside try.
funcinfo_catch_int: .long 0x19930520, 2, unwind_catch_int, 1, try_catch_int, 0, 0
unwind_catch_int: .long -1, 0, 0, funclet_dtor
try_catch_int: .long 0, 1, 2, 2, catches_catch_int
catches_catch_int:
    .long 0, td_double, -0x18, funclet_catch_double
    .long 0, td_int, -0x18, funclet_catch_int

# Test 2.
funcinfo_outer: .long 0x19930520, 1, unwind_outer, 1, try_outer, 0, 0
unwind_outer: .long -1, 0
try_outer: .long 0, 0, 1, 1, catches_outer
catches_outer: .long 0, 0, 0, funclet_catch_all
funcinfo_outer_av: .long 0x19930520, 1, unwind_outer, 1, try_outer_av, 0, 0
try_outer_av: .long 0, 0, 1, 1, catches_outer_av
catches_outer_av: .long 0, 0, 0, funclet_cxx_av
funcinfo_inner: .long 0x19930520, 1, unwind_inner, 0, 0, 0, 0
unwind_inner: .long -1, funclet_dtor

# Test 3.
funcinfo_rethrow_outer: .long 0x19930520, 1, unwind_rethrow_outer, 1, try_rethrow_outer, 0, 0
unwind_rethrow_outer: .long -1, 0
try_rethrow_outer: .long 0, 0, 1, 1, catches_rethrow_outer
catches_rethrow_outer: .long 0, td_int, -0x18, funclet_rethrow_outer
funcinfo_rethrow_inner: .long 0x19930520, 1, unwind_rethrow_inner, 1, try_rethrow_inner, 0, 0
unwind_rethrow_inner: .long -1, 0
try_rethrow_inner: .long 0, 0, 1, 1, catches_rethrow_inner
catches_rethrow_inner: .long 0, td_int, -0x18, funclet_rethrow_inner

# Scope tables: previous level, filter (0 = __finally), handler.
scopetable_seh:
    .long -1, seh_filter, seh_handler
    .long 0, 0, seh_finally
scopetable_continue:
    .long -1, continue_filter, continue_handler
scopetable_av_read:
    .long -1, av_read_filter, av_read_handler
scopetable_av_continue:
    .long -1, av_continue_filter, av_continue_handler
scopetable_divide:
    .long -1, divide_filter, divide_handler

# ---- imports ----
.align 4
.globl idesc
idesc:
    .long kernel_int - BASE, 0, 0, kernel_name - BASE, kernel_iat - BASE
    .long crt_int - BASE, 0, 0, crt_name - BASE, crt_iat - BASE
    .long 0, 0, 0, 0, 0
kernel_int: .long hn_exit - BASE, hn_write - BASE, hn_stdh - BASE, hn_raise - BASE, 0
kernel_iat:
iat_ExitProcess: .long hn_exit - BASE
iat_WriteFile: .long hn_write - BASE
iat_GetStdHandle: .long hn_stdh - BASE
iat_RaiseException: .long hn_raise - BASE
    .long 0
crt_int: .long hn_throw - BASE, hn_frame - BASE, hn_seh - BASE, 0
crt_iat:
iat__CxxThrowException: .long hn_throw - BASE
iat___CxxFrameHandler: .long hn_frame - BASE
iat__except_handler3: .long hn_seh - BASE
    .long 0
.align 2
hn_exit: .short 0
    .asciz "ExitProcess"
.align 2
hn_write: .short 0
    .asciz "WriteFile"
.align 2
hn_stdh: .short 0
    .asciz "GetStdHandle"
.align 2
hn_raise: .short 0
    .asciz "RaiseException"
.align 2
hn_throw: .short 0
    .asciz "_CxxThrowException"
.align 2
hn_frame: .short 0
    .asciz "__CxxFrameHandler"
.align 2
hn_seh: .short 0
    .asciz "_except_handler3"
kernel_name: .asciz "KERNEL32.dll"
crt_name: .asciz "MSVCRT20.dll"
.globl image_end
image_end:
"""


def assemble():
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / "eh.s").write_text(ASM)
        subprocess.run(["as", "--32", "-o", tmp / "eh.o", tmp / "eh.s"], check=True)
        script = tmp / "link.ld"
        script.write_text(
            "SECTIONS { . = 0x%x; .text : { *(.text) *(.data) } /DISCARD/ : { *(*) } }\n" % TEXT_VA
        )
        subprocess.run(["ld", "-m", "elf_i386", "-T", script, "-e", "start", "-o", tmp / "eh.elf", tmp / "eh.o"], check=True)
        subprocess.run(["objcopy", "-O", "binary", "-j", ".text", tmp / "eh.elf", tmp / "eh.bin"], check=True)
        symbols = {}
        for line in subprocess.run(["nm", tmp / "eh.elf"], capture_output=True, text=True, check=True).stdout.splitlines():
            parts = line.split()
            if len(parts) == 3:
                symbols[parts[2]] = int(parts[0], 16)
        return (tmp / "eh.bin").read_bytes(), symbols


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
        3, 0,
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
