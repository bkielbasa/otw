//! MSVCRT.DLL, the C runtime of Windows itself, as 32-bit programs
//! (installers such as 7-Zip's, programs built with Visual C++ 6 or
//! MinGW) import it.
//!
//! It is put together from what OTW already has: MSVCRT20's start-up,
//! exceptions and exit (the same entry points under the same names), the
//! older C library functions (`crt`), and the Universal C Runtime (`ucrt`)
//! for the rest, whose functions take the same arguments. MSVCRT's own
//! additions are here: `__set_app_type`, `__wgetmainargs`, the printf
//! functions whose arguments follow the format on the stack, and the
//! variables it exports (`_acmdln` and others: `data_import`).
//!
//! Every function is `cdecl`: the caller removes the arguments. The UCRT's
//! floating-point functions are not offered: a 32-bit caller expects the
//! result on the x87 stack (the older C library's `atof` and `strtod` put
//! it there).

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::{
    kernel::{
        memory::{ApiOutcome, GuestMemory},
        pe::ImportName,
        syscall::DataImport,
    },
    win32::{ucrt, ApiContext},
};

/// Whether `dll` is MSVCRT.DLL.
pub fn is_msvcrt(dll: &str) -> bool {
    dll.eq_ignore_ascii_case("msvcrt.dll") || dll.eq_ignore_ascii_case("msvcrt")
}

/// Stack slots read after the fixed arguments of a function that takes a
/// variable number (the caller's frame beyond them is read, not used).
const VARIADIC_SLOTS: usize = 24;

/// MSVCRT's printf functions that take their values after the format:
/// (fixed arguments, UCRT function, where the buffer, its size and the
/// format are among the fixed ones).
fn variadic(name: &str) -> Option<(usize, Printf)> {
    Some(match name {
        "swprintf" | "_swprintf" => (2, Printf::Buffer { wide: true, counted: false }),
        "_snwprintf" => (3, Printf::Buffer { wide: true, counted: true }),
        "wprintf" => (1, Printf::Stream { wide: true, stream: false }),
        "printf" => (1, Printf::Stream { wide: false, stream: false }),
        "fwprintf" => (2, Printf::Stream { wide: true, stream: true }),
        "fprintf" => (2, Printf::Stream { wide: false, stream: true }),
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Printf {
    /// Into a buffer, with its size first when `counted`.
    Buffer { wide: bool, counted: bool },
    /// To a stream: the one given first, or standard output.
    Stream { wide: bool, stream: bool },
}

/// MSVCRT's `va_list` printf functions with the arguments in a list
/// already: (fixed arguments, wide, counted).
fn listed(name: &str) -> Option<(bool, bool)> {
    Some(match name {
        "vswprintf" => (true, false),
        "_vsnwprintf" => (true, true),
        _ => return None,
    })
}

/// Functions of MSVCRT's own besides those of MSVCRT20, the C library and
/// the UCRT.
fn own_arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "__p__wcmdln" | "__p___initenv" | "__p___winitenv" | "_crt_debugger_hook" | "__lconv_init"
        | "??1type_info@@UAE@XZ" => 0,
        "__set_app_type" | "_amsg_exit" | "_lock" | "_unlock" | "atexit" | "_wcsupr" | "_wtol" => 1,
        "wcsstr" | "wcschr" | "wcsrchr" | "wcscat" | "wcsspn" | "wcscspn" => 2,
        "mbstowcs" | "_itow" => 3,
        "__wgetmainargs" => 5,
        "vswprintf" => 3,
        "_vsnwprintf" => 4,
        _ => return None,
    })
}

/// Functions the UCRT has that a 32-bit caller cannot use: their
/// floating-point arguments or result are in XMM registers there.
fn floating_point(name: &str) -> bool {
    matches!(
        name,
        "atof" | "strtod" | "strtof" | "wcstod" | "wcstof" | "ldexp" | "scalbn" | "scalbnf" | "frexp" | "modf"
            | "modff" | "_difftime64" | "nan" | "nanf" | "_dtest" | "_fdtest" | "_ldtest" | "fesetround"
            | "fegetround" | "_control87" | "_controlfp_s" | "_clearfp"
    ) || ucrt::is_math(name)
}

/// How many stack slots an MSVCRT function reads; `None` if OTW lacks it.
pub fn arg_count(name: &str) -> Option<usize> {
    if let Some((fixed, _)) = variadic(name) {
        return Some(fixed + VARIADIC_SLOTS);
    }
    own_arg_count(name)
        .or_else(|| super::msvcrt20::arg_count(name))
        .or_else(|| (!floating_point(name)).then(|| ucrt::arg_count(name)).flatten())
}

/// Call an MSVCRT function from 32-bit code.
pub fn call(ctx: &ApiContext, memory: &mut dyn GuestMemory, name: &ImportName, args: &[u32]) -> Option<ApiOutcome> {
    let text = name.as_str()?;
    let outcome = if let Some((fixed, kind)) = variadic(text) {
        printf(ctx, memory, kind, &args[..fixed], &args[fixed..])
    } else if own_arg_count(text).is_some() {
        own(ctx, memory, text, args)
    } else if super::msvcrt20::arg_count(text).is_some() {
        super::msvcrt20::dispatch(ctx, memory, name, args)
    } else if !floating_point(text) && ucrt::arg_count(text).is_some() {
        let wide: Vec<u64> = args.iter().map(|&a| u64::from(a)).collect();
        ucrt::call(ctx, memory, text, &wide)
    } else {
        None
    }?;
    // cdecl: the caller removes the arguments.
    Some(ApiOutcome { arg_count: 0, ..outcome })
}

fn own(ctx: &ApiContext, memory: &mut dyn GuestMemory, name: &str, args: &[u32]) -> Option<ApiOutcome> {
    let ok = |value: u32| Some(ApiOutcome::cdecl(value));
    match name {
        // type_info's destructor (this in ECX): nothing to free.
        "__set_app_type" | "_lock" | "_unlock" | "_crt_debugger_hook" | "__lconv_init" | "??1type_info@@UAE@XZ" => ok(0),
        "wcsstr" | "wcschr" | "wcsrchr" | "wcscat" | "wcsspn" | "wcscspn" | "_wcsupr" | "_wtol" | "mbstowcs" | "_itow" => {
            ok(wide_string(memory, name, args))
        }
        "atexit" => {
            // Run at exit as `_onexit` does: 0 is success.
            let registered = super::msvcrt20::dispatch(ctx, memory, &ImportName::ByName("_onexit".into()), args)?;
            ok(if registered.value == 0 { u32::MAX } else { 0 })
        }
        "_amsg_exit" => {
            eprintln!("[msvcrt] run-time error R60{:02}", args[0]);
            Some(ApiOutcome { value: 0, wide: None, arg_count: 0, exit_code: Some(255) })
        }
        "__p__wcmdln" => ok(wide_globals(ctx, memory).command_line),
        "__p___initenv" | "__p___winitenv" => ok(wide_globals(ctx, memory).environment),
        "__wgetmainargs" => {
            // (&argc, &wargv, &wenviron, expand wildcards, &startup info)
            let globals = wide_globals(ctx, memory);
            let _ = memory.write_u32(args[0], 1);
            let _ = memory.write_u32(args[1], globals.argv);
            let _ = memory.write_u32(args[2], globals.environment_block);
            ok(0)
        }
        "vswprintf" | "_vsnwprintf" => {
            let (wide, counted) = listed(name)?;
            let (buffer, count, format, list) =
                if counted { (args[0], args[1], args[2], args[3]) } else { (args[0], i32::MAX as u32, args[1], args[2]) };
            let function = if wide { "__stdio_common_vswprintf" } else { "__stdio_common_vsprintf" };
            // Legacy: no terminating zero when it fills the buffer.
            let outcome = ucrt::call(ctx, memory, function, &[1, buffer.into(), count.into(), format.into(), 0, list.into()])?;
            ok(outcome.value)
        }
        _ => None,
    }
}

fn read_wide(memory: &mut dyn GuestMemory, va: u32) -> Vec<u16> {
    crate::win32::unicode::read_wstr(memory, va)
}

fn write_wide(memory: &mut dyn GuestMemory, va: u32, units: &[u16]) {
    let bytes: Vec<u8> = units.iter().chain(&[0]).flat_map(|u| u.to_le_bytes()).collect();
    let _ = memory.write_bytes(va, &bytes);
}

/// The wide string functions the UCRT part lacks.
fn wide_string(memory: &mut dyn GuestMemory, name: &str, args: &[u32]) -> u32 {
    match name {
        "wcsstr" => {
            let (text, part) = (read_wide(memory, args[0]), read_wide(memory, args[1]));
            if part.is_empty() {
                return args[0];
            }
            text.windows(part.len()).position(|w| w == part.as_slice()).map_or(0, |i| args[0] + 2 * i as u32)
        }
        "wcschr" | "wcsrchr" => {
            let text = read_wide(memory, args[0]);
            let unit = args[1] as u16;
            // The terminating zero is part of the string.
            let found = if unit == 0 {
                Some(text.len())
            } else if name == "wcschr" {
                text.iter().position(|&u| u == unit)
            } else {
                text.iter().rposition(|&u| u == unit)
            };
            found.map_or(0, |i| args[0] + 2 * i as u32)
        }
        "wcscat" => {
            let length = read_wide(memory, args[0]).len() as u32;
            let tail = read_wide(memory, args[1]);
            write_wide(memory, args[0] + 2 * length, &tail);
            args[0]
        }
        "wcsspn" | "wcscspn" => {
            let (text, set) = (read_wide(memory, args[0]), read_wide(memory, args[1]));
            let inside = name == "wcsspn";
            text.iter().position(|u| set.contains(u) != inside).unwrap_or(text.len()) as u32
        }
        "_wcsupr" => {
            let text: Vec<u16> = read_wide(memory, args[0])
                .into_iter()
                .map(|u| char::from_u32(u as u32).map_or(u, |c| {
                    let mut upper = c.to_uppercase();
                    match (upper.next(), upper.next()) {
                        (Some(one), None) if (one as u32) < 0x10000 => one as u16,
                        _ => u,
                    }
                }))
                .collect();
            write_wide(memory, args[0], &text);
            args[0]
        }
        "_wtol" => {
            let text = String::from_utf16_lossy(&read_wide(memory, args[0]));
            let text = text.trim_start();
            let (negative, digits) = match text.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, text.strip_prefix('+').unwrap_or(text)),
            };
            let value = digits.chars().take_while(char::is_ascii_digit).fold(0i32, |v, c| v.wrapping_mul(10).wrapping_add(c as i32 - '0' as i32));
            (if negative { value.wrapping_neg() } else { value }) as u32
        }
        "mbstowcs" => {
            // (destination or NULL, source, most units): the count.
            let units = crate::win32::unicode::ansi_to_wide(&crate::win32::read_cstr(memory, args[1]));
            if args[0] == 0 {
                return units.len() as u32;
            }
            let n = units.len().min(args[2] as usize);
            let mut out = units[..n].to_vec();
            if n < args[2] as usize {
                out.push(0);
            }
            let bytes: Vec<u8> = out.iter().flat_map(|u| u.to_le_bytes()).collect();
            let _ = memory.write_bytes(args[0], &bytes);
            n as u32
        }
        "_itow" => {
            // (value, buffer, radix): negative only in base 10.
            let radix = if (2..=36).contains(&args[2]) { args[2] } else { 10 };
            let negative = radix == 10 && (args[0] as i32) < 0;
            let mut magnitude = if negative { (args[0] as i32).unsigned_abs() } else { args[0] };
            let mut digits = Vec::new();
            loop {
                digits.push(char::from_digit(magnitude % radix, radix).unwrap() as u16);
                magnitude /= radix;
                if magnitude == 0 {
                    break;
                }
            }
            if negative {
                digits.push(b'-' as u16);
            }
            digits.reverse();
            write_wide(memory, args[1], &digits);
            args[1]
        }
        _ => 0,
    }
}

/// `printf`, `swprintf` and the like: their values after the format are
/// made into a list as `va_list` is in 32-bit code (one slot each, two
/// for a `double`).
fn printf(ctx: &ApiContext, memory: &mut dyn GuestMemory, kind: Printf, fixed: &[u32], rest: &[u32]) -> Option<ApiOutcome> {
    let list = ctx.shared.borrow_mut().heap.alloc(rest.len() as u32 * 4)?;
    let bytes: Vec<u8> = rest.iter().flat_map(|v| v.to_le_bytes()).collect();
    let _ = memory.write_bytes(list, &bytes);
    let call = |memory: &mut dyn GuestMemory, function: &str, args: &[u64]| ucrt::call(ctx, memory, function, args);
    let outcome = match kind {
        Printf::Buffer { wide, counted } => {
            let (buffer, count, format) = if counted { (fixed[0], fixed[1], fixed[2]) } else { (fixed[0], i32::MAX as u32, fixed[1]) };
            let function = if wide { "__stdio_common_vswprintf" } else { "__stdio_common_vsprintf" };
            call(memory, function, &[1, buffer.into(), count.into(), format.into(), 0, list.into()])
        }
        Printf::Stream { wide, stream } => {
            let (file, format) = if stream {
                (u64::from(fixed[0]), fixed[1])
            } else {
                (call(memory, "__acrt_iob_func", &[1])?.wide.unwrap_or(0), fixed[0])
            };
            let function = if wide { "__stdio_common_vfwprintf" } else { "__stdio_common_vfprintf" };
            call(memory, function, &[0, file, format.into(), 0, list.into()])
        }
    };
    ctx.shared.borrow_mut().heap.free(list);
    outcome
}

/// What the wide start-up functions hand out, made once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WideGlobals {
    /// `wchar_t *_wcmdln`'s address.
    pub command_line: u32,
    /// `{ command line, NULL }`: the arguments.
    pub argv: u32,
    /// `{ NULL }`: the environment (programs ask the system for it).
    pub environment_block: u32,
    /// The address of a pointer to it (`__winitenv`).
    pub environment: u32,
}

fn wide_globals(ctx: &ApiContext, memory: &mut dyn GuestMemory) -> WideGlobals {
    let current = ctx.shared.borrow().msvcrt;
    if current.command_line != 0 {
        return current;
    }
    let ansi = crate::win32::read_cstr(memory, ctx.command_line_va);
    let units: Vec<u16> = crate::win32::unicode::ansi_to_wide(&ansi).into_iter().chain([0]).collect();
    let globals = {
        let mut shared = ctx.shared.borrow_mut();
        let text = shared.bump_alloc(units.len() as u32 * 2);
        let globals = WideGlobals {
            command_line: shared.bump_alloc(4),
            argv: shared.bump_alloc(8),
            environment_block: shared.bump_alloc(4),
            environment: shared.bump_alloc(4),
        };
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let _ = memory.write_bytes(text, &bytes);
        let _ = memory.write_u32(globals.command_line, text);
        let _ = memory.write_u32(globals.argv, text);
        let _ = memory.write_u32(globals.argv + 4, 0);
        let _ = memory.write_u32(globals.environment_block, 0);
        let _ = memory.write_u32(globals.environment, globals.environment_block);
        shared.msvcrt = globals;
        globals
    };
    globals
}

/// The variables MSVCRT exports, as the loader places them.
pub fn data_import(name: &str) -> Option<DataImport> {
    Some(match name {
        "_acmdln" => DataImport::CommandLine,
        "_wcmdln" => DataImport::WideCommandLine,
        "__argv" => DataImport::Arguments,
        "__wargv" => DataImport::WideArguments,
        "__argc" => DataImport::Value(1),
        // 0: files open in the default (text) mode.
        "_fmode" | "_commode" | "_adjust_fdiv" | "__mb_cur_max" => DataImport::Value(if name == "__mb_cur_max" { 1 } else { 0 }),
        "_environ" | "_wenviron" | "__initenv" | "__winitenv" => DataImport::EmptyList,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use crate::platform as std;
    #[allow(unused_imports)]
    use crate::platform::prelude::*;
    use super::*;

    #[test]
    fn the_seven_zip_installer_imports_are_there() {
        // 7-Zip 24.09's installer (7z2409.exe) imports these.
        for name in [
            "_exit", "_XcptFilter", "__getmainargs", "_initterm", "__setusermatherr", "_adjust_fdiv",
            "__p__commode", "__p__fmode", "__set_app_type", "_except_handler3", "_controlfp", "memcpy", "memcmp",
            "memmove", "malloc", "free", "exit", "memset",
        ] {
            assert!(arg_count(name).is_some(), "{name}");
        }
        assert_eq!(data_import("_acmdln"), Some(DataImport::CommandLine));
    }

    #[test]
    fn the_seven_zip_file_manager_imports_are_there() {
        // 7-Zip 24.09's 7zFM.exe imports these.
        for name in [
            "??1type_info@@UAE@XZ", "_beginthreadex", "__CxxFrameHandler", "_CxxThrowException", "__dllonexit",
            "_onexit", "_purecall", "rand", "srand", "strcmp", "strlen", "?terminate@@YAXXZ", "wcscmp", "wcslen",
            "wcsstr", "wcschr", "wcsrchr", "wcsncpy", "wcscpy", "wcscat", "_wcsicmp", "_wcsnicmp", "towupper",
        ] {
            assert!(arg_count(name).is_some(), "{name}");
        }
    }

    /// Flat memory from address 0.
    struct Flat(Vec<u8>);
    impl GuestMemory for Flat {
        fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
            self.0.get(va as usize..va as usize + len).map(<[u8]>::to_vec)
        }
        fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()> {
            self.0.get_mut(va as usize..va as usize + bytes.len())?.copy_from_slice(bytes);
            Some(())
        }
        fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
            self.write_bytes(va, &value.to_le_bytes())
        }
        fn ptr_size(&self) -> u32 {
            4
        }
        fn current_thread(&self) -> u32 {
            1
        }
        fn current_tib(&self) -> u32 {
            0
        }
    }

    fn put(memory: &mut Flat, va: u32, text: &str) {
        let units: Vec<u16> = text.encode_utf16().collect();
        write_wide(memory, va, &units);
    }

    fn get(memory: &mut Flat, va: u32) -> String {
        String::from_utf16_lossy(&read_wide(memory, va))
    }

    #[test]
    fn wide_string_functions() {
        let mut m = Flat(vec![0; 0x400]);
        put(&mut m, 0x100, "C:\\Program Files\\7-Zip");
        put(&mut m, 0x200, "Files");
        assert_eq!(wide_string(&mut m, "wcsstr", &[0x100, 0x200]), 0x100 + 2 * 11);
        put(&mut m, 0x200, "nothing");
        assert_eq!(wide_string(&mut m, "wcsstr", &[0x100, 0x200]), 0);
        assert_eq!(wide_string(&mut m, "wcschr", &[0x100, '\\' as u32]), 0x100 + 4);
        assert_eq!(wide_string(&mut m, "wcsrchr", &[0x100, '\\' as u32]), 0x100 + 2 * 16);
        assert_eq!(wide_string(&mut m, "wcschr", &[0x100, 0]), 0x100 + 2 * 22);
        put(&mut m, 0x200, "\\7z.exe");
        assert_eq!(wide_string(&mut m, "wcscat", &[0x100, 0x200]), 0x100);
        assert_eq!(get(&mut m, 0x100), "C:\\Program Files\\7-Zip\\7z.exe");
        put(&mut m, 0x200, "C:\\");
        assert_eq!(wide_string(&mut m, "wcsspn", &[0x100, 0x200]), 3);
        put(&mut m, 0x200, " ");
        assert_eq!(wide_string(&mut m, "wcscspn", &[0x100, 0x200]), 10);
        wide_string(&mut m, "_wcsupr", &[0x100]);
        assert_eq!(get(&mut m, 0x100), "C:\\PROGRAM FILES\\7-ZIP\\7Z.EXE");
        put(&mut m, 0x200, "  -42abc");
        assert_eq!(wide_string(&mut m, "_wtol", &[0x200]) as i32, -42);
        assert_eq!(wide_string(&mut m, "_itow", &[(-305i32) as u32, 0x300, 10]), 0x300);
        assert_eq!(get(&mut m, 0x300), "-305");
        wide_string(&mut m, "_itow", &[255, 0x300, 16]);
        assert_eq!(get(&mut m, 0x300), "ff");
        let _ = m.write_bytes(0x80, b"abc\0");
        assert_eq!(wide_string(&mut m, "mbstowcs", &[0, 0x80, 0]), 3);
        assert_eq!(wide_string(&mut m, "mbstowcs", &[0x300, 0x80, 10]), 3);
        assert_eq!(get(&mut m, 0x300), "abc");
    }

    #[test]
    fn calls_are_cdecl() {
        let ctx = ApiContext::default();
        let mut m = Flat(vec![0; 0x400]);
        put(&mut m, 0x100, "7-Zip");
        put(&mut m, 0x200, "Zip");
        let found = call(&ctx, &mut m, &ImportName::ByName("wcsstr".into()), &[0x100, 0x200]).unwrap();
        assert_eq!((found.value, found.arg_count), (0x104, 0));
        // From the UCRT part.
        let length = call(&ctx, &mut m, &ImportName::ByName("wcslen".into()), &[0x100]).unwrap();
        assert_eq!((length.value, length.arg_count), (5, 0));
    }

    #[test]
    fn floating_point_functions_are_not_offered() {
        // The UCRT's take and give doubles in XMM registers.
        assert_eq!(arg_count("sqrt"), None);
        assert_eq!(arg_count("wcstod"), None);
        assert_eq!(arg_count("_hypot"), None);
        // The older C library's give theirs on the x87 stack.
        assert!(arg_count("atof").is_some());
        assert!(arg_count("wcslen").is_some());
    }

    #[test]
    fn variadic_functions_read_their_values() {
        assert_eq!(arg_count("swprintf"), Some(2 + VARIADIC_SLOTS));
        assert_eq!(arg_count("_snwprintf"), Some(3 + VARIADIC_SLOTS));
        assert_eq!(arg_count("printf"), Some(1 + VARIADIC_SLOTS));
    }

    #[test]
    fn exported_variables() {
        assert_eq!(data_import("__argc"), Some(DataImport::Value(1)));
        assert_eq!(data_import("_fmode"), Some(DataImport::Value(0)));
        assert_eq!(data_import("_wcmdln"), Some(DataImport::WideCommandLine));
        assert_eq!(data_import("printf"), None);
    }
}
