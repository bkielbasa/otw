//! The Universal C Runtime (`ucrtbase.dll`, imported through the
//! `api-ms-win-crt-*` API sets) that programs built with Visual Studio 2015
//! and later use: start-up and exit, the heap, strings and conversions,
//! formatted input and output, streams and file descriptors, time,
//! locales (the "C" locale), and the maths library.
//!
//! Arguments arrive in full width; floating-point ones in XMM registers
//! (x64), where floating-point results also go. Formatted output takes the
//! `va_list` the inline `printf` family builds: pointer-sized slots.

use std::collections::HashMap;
use std::io::Write;

use crate::{
    kernel::pe::ImportName,
    win32::{crt, read_cstr, unicode, ApiContext, ApiOutcome, GuestMemory},
};

const EINVAL: u32 = 22;
const ERANGE: u32 = 34;
const ENOENT: u32 = 2;
const EBADF: u32 = 9;
const STRUNCATE: u32 = 80;
const TRUNCATE: u64 = u64::MAX;

/// A stream (`FILE`): a descriptor plus the C library's read-ahead.
#[derive(Clone, Debug, Default)]
struct Stream {
    fd: i32,
    eof: bool,
    error: bool,
    pushed: Vec<u8>,
}

/// A file descriptor: a host file handle, or one of the standard three.
#[derive(Clone, Copy, Debug)]
enum Fd {
    Std(u32),
    File(u32),
}

#[derive(Debug, Default)]
pub struct State {
    statics: HashMap<String, u32>,
    atexit: Vec<u64>,
    streams: HashMap<u32, Stream>,
    fds: HashMap<i32, Fd>,
    aligned: HashMap<u32, u32>,
    seed: u32,
    started: Option<std::time::Instant>,
    strings: HashMap<String, u32>,
}

/// The functions implemented here and how many arguments each takes.
pub fn arg_count(name: &str) -> Option<usize> {
    let name = name.strip_prefix("_o_").unwrap_or(name);
    Some(match name {
        "_get_initial_narrow_environment" | "_get_initial_wide_environment" | "__p___argc" | "__p___argv"
        | "__p___wargv" | "__p__environ" | "__p__wenviron" | "__p__commode" | "_get_wide_winmain_command_line"
        | "_get_narrow_winmain_command_line" | "_initialize_narrow_environment" | "_initialize_wide_environment"
        | "_cexit" | "_c_exit" | "abort" | "terminate" | "_errno" | "__doserrno" | "_invalid_parameter_noinfo"
        | "_invalid_parameter_noinfo_noreturn" | "_purecall" | "localeconv" | "__pctype_func" | "___lc_codepage_func"
        | "___lc_collate_cp_func" | "___lc_locale_name_func" | "___mb_cur_max_func" | "_lock_locales"
        | "_unlock_locales" | "rand" | "clock" | "_tzset" | "__timezone" | "__tzname" | "__daylight" | "_getpid"
        | "_clearfp" | "_getdrives" | "_resetstkoflw" | "fegetround" | "_Getdays" | "_Getmonths" | "_Gettnames"
        | "_W_Getdays" | "_W_Getmonths" | "_W_Gettnames" | "_endthreadex" | "_get_invalid_parameter_handler"
        | "__acrt_iob_func" | "_set_app_type" | "_configure_narrow_argv" | "_configure_wide_argv" | "_set_fmode"
        | "_configthreadlocale" | "_set_new_mode" | "__setusermatherr" | "_set_invalid_parameter_handler"
        | "_register_thread_local_exe_atexit_callback" | "_callnewh" | "_set_new_handler" | "_initialize_onexit_table"
        | "_execute_onexit_table" | "_crt_atexit" | "exit" | "_exit" | "malloc" | "_malloc_base" | "free"
        | "_free_base" | "_aligned_free" | "strlen" | "wcslen" | "_strdup" | "_wcsdup" | "strerror" | "atoi" | "atol"
        | "_atoi64" | "atof" | "_wtoi" | "tolower" | "toupper" | "towlower" | "towupper" | "isalnum" | "isalpha"
        | "isdigit" | "islower" | "isupper" | "isprint" | "isspace" | "isxdigit" | "iswdigit" | "iswspace"
        | "iswalpha" | "srand" | "rand_s" | "abs" | "llabs" | "_time64" | "_gmtime64" | "_localtime64" | "_mktime64"
        | "asctime" | "_ftime64" | "getenv" | "_wgetenv" | "_putenv" | "_wputenv" | "_free_locale" | "_close" | "_isatty"
        | "_dup" | "_get_osfhandle" | "puts" | "putchar" | "fclose" | "fflush" | "fgetc" | "fgetwc" | "feof"
        | "ferror" | "rewind" | "_fileno" | "_lock_file" | "_unlock_file" | "perror" | "remove" | "_wremove"
        | "_wrmdir" | "_chdir" | "_wchdir" | "_mkdir" | "raise" | "fesetround" | "_wcslwr" | "nan" | "nanf"
        | "_dtest" | "_fdtest" | "_ldtest" | "__std_exception_destroy" => 1,
        "_set_abort_behavior" | "_register_onexit_function" | "_initterm" | "_initterm_e"
        | "calloc" | "_calloc_base" | "realloc" | "_aligned_malloc" | "strcmp" | "wcscmp" | "_stricmp"
        | "_wcsicmp" | "strnlen" | "wcsnlen" | "__strncnt" | "strcpy" | "wcscpy" | "strcat" | "strcspn" | "strspn"
        | "strpbrk" | "wcspbrk" | "_towlower_l" | "_towupper_l" | "_gmtime64_s" | "_localtime64_s" | "setlocale"
        | "_wsetlocale" | "_create_locale" | "_putenv_s" | "_wputenv_s" | "fopen" | "_wfopen"
        | "fputc" | "fputs" | "fputwc" | "fgetpos" | "fsetpos" | "ungetc" | "ungetwc" | "setbuf" | "ftell"
        | "_ftelli64" | "_access" | "_waccess" | "_wrename" | "_wchmod" | "_setmode" | "_open_osfhandle"
        | "_fstat64i32" | "_wstat64" | "_wstat64i32" | "_fdopen" | "_wgetcwd" | "_mktemp_s" | "_wmktemp_s"
        | "_chsize" | "__std_exception_copy" | "_rotl" | "div"
        | "_difftime64" | "_wcsupr_s" | "ldexp" | "scalbn" | "scalbnf" | "frexp" | "modf" | "modff"
        | "_control87" => 2,
        "_set_errno" | "_set_doserrno" => 1,
        "_aligned_realloc" | "_aligned_offset_malloc"
        | "_recalloc" | "strcpy_s" | "wcscpy_s" | "strcat_s" | "wcscat_s" | "strncmp" | "wcsncmp" | "_strnicmp"
        | "_wcsnicmp" | "strncpy" | "wcsncpy" | "strncat" | "strtol" | "strtoul" | "strtoll" | "strtoull"
        | "_strtoui64" | "wcstol" | "wcstoul" | "wcstoull" | "_wcstoui64" | "_i64toa" | "_ui64toa" | "wcstombs"
        | "wcstok_s" | "_wcsicmp_l" | "strerror_s" | "_wfullpath" | "_fullpath" | "_read"
        | "_write" | "_lseek" | "_wgetdcwd" | "_fsopen" | "_wfsopen" | "freopen" | "fseek" | "_fseeki64" | "fgets"
        | "_wassert" | "_controlfp_s" | "lldiv" | "imaxdiv" | "_mbsdec"
        | "_open" | "_wopen" => 3,
        "_mbsinc" => 1,
        "strtod" | "strtof" | "wcstod" | "wcstof" => 2,
        "fwrite" | "fread" | "_ltoa_s" | "_i64tow_s" | "mbsrtowcs" | "setvbuf" | "_get_stream_buffer_pointers"
        | "qsort" | "strftime" | "wcsftime" => 4,
        "_sopen_s" | "_wsopen_s" | "bsearch" | "_Strftime" | "_Wcsftime"
        | "__stdio_common_vfprintf" | "__stdio_common_vfwprintf" | "__stdio_common_vfprintf_s"
        | "__stdio_common_vfwprintf_s" | "__stdio_common_vfscanf" | "__stdio_common_vfwscanf" => 5,
        "_beginthreadex" | "__stdio_common_vsprintf" | "__stdio_common_vswprintf" | "__stdio_common_vsprintf_s"
        | "__stdio_common_vswprintf_s" | "__stdio_common_vsscanf" | "__stdio_common_vswscanf" => 6,
        "__stdio_common_vsnprintf_s" | "__stdio_common_vsnwprintf_s" => 7,
        "_splitpath_s" => 9,
        // Maths: arguments in XMM registers (and an integer or pointer).
        "fabs" | "sqrt" | "sqrtf" | "sin" | "sinf" | "cos" | "cosf" | "tan" | "tanf" | "asin" | "asinf" | "acos"
        | "acosf" | "atan" | "atanf" | "sinh" | "sinhf" | "cosh" | "coshf" | "tanh" | "tanhf" | "asinh" | "asinhf"
        | "acosh" | "acoshf" | "atanh" | "atanhf" | "exp" | "expf" | "exp2" | "exp2f" | "expm1" | "expm1f" | "log"
        | "logf" | "log10" | "log10f" | "log2" | "log2f" | "log1p" | "log1pf" | "cbrt" | "cbrtf" | "ceil" | "ceilf"
        | "floor" | "floorf" | "trunc" | "truncf" | "round" | "roundf" | "rint" | "rintf" | "nearbyint"
        | "nearbyintf" | "lrint" | "lrintf" | "llrint" | "llrintf" | "lround" | "lroundf" | "llround" | "llroundf"
        | "llroundl" | "ilogb" | "ilogbf" | "erff" | "erf" | "lgamma" | "lgammaf" | "lgammal" | "_dclass"
        | "_fdclass" | "_ldclass" | "_dsign" | "_fdsign" | "_ldsign" | "_isnan" | "_finite" | "_fpclass" => 1,
        "atan2" | "atan2f" | "pow" | "powf" | "fmod" | "fmodf" | "hypot" | "_hypot" | "_hypotf" | "copysign"
        | "copysignf" | "fmax" | "fmaxf" | "fmin" | "fminf" | "remainder" | "remainderf" | "nextafter"
        | "nextafterf" | "nexttoward" => 2,
        "fma" | "fmaf" => 3,
        "_seh_filter_exe" | "_seh_filter_dll" => 2,
        "getenv_s" | "memcpy_s" | "wmemcpy_s" | "strncpy_s" | "wcsncpy_s" => 4,
        "_makepath_s" | "_wsopen_dispatch" => 6,
        _ => return None,
    })
}

fn state<R>(ctx: &ApiContext, f: impl FnOnce(&mut State) -> R) -> R {
    f(&mut ctx.shared.borrow_mut().ucrt)
}

fn alloc(ctx: &ApiContext, size: u64) -> u32 {
    if size > u32::MAX as u64 {
        return 0;
    }
    ctx.shared.borrow_mut().heap.alloc(size as u32).unwrap_or(0)
}

fn release(ctx: &ApiContext, va: u32) {
    if va != 0 {
        ctx.shared.borrow_mut().heap.free(va);
    }
}

fn win32(ctx: &ApiContext, memory: &mut dyn GuestMemory, dll: &str, name: &str, args: &[u32]) -> u32 {
    crate::win32::dispatch_with_memory(ctx, memory, dll, &ImportName::ByName(name.into()), args, 0)
        .map(|o| o.value)
        .unwrap_or(0)
}

fn set_errno(ctx: &ApiContext, memory: &mut dyn GuestMemory, value: u32) {
    let block = static_block(ctx, memory, "errno", 8);
    let _ = memory.write_u32(block, value);
}

/// A zeroed block made once and kept for the process's life.
fn static_block(ctx: &ApiContext, memory: &mut dyn GuestMemory, key: &str, size: u32) -> u32 {
    if let Some(&va) = state(ctx, |s| s.statics.get(key).copied()).as_ref() {
        return va;
    }
    let va = alloc(ctx, size as u64);
    let _ = memory.write_bytes(va, &vec![0; size as usize]);
    state(ctx, |s| s.statics.insert(key.into(), va));
    va
}

/// A string made once and kept (static results such as `strerror`'s).
fn static_cstr(ctx: &ApiContext, memory: &mut dyn GuestMemory, text: &[u8]) -> u32 {
    let key = format!("a:{}", String::from_utf8_lossy(text));
    if let Some(va) = state(ctx, |s| s.strings.get(&key).copied()) {
        return va;
    }
    let va = alloc(ctx, text.len() as u64 + 1);
    put_c(memory, va, text);
    state(ctx, |s| s.strings.insert(key, va));
    va
}

fn static_wstr(ctx: &ApiContext, memory: &mut dyn GuestMemory, text: &[u16]) -> u32 {
    let key = format!("w:{}", String::from_utf16_lossy(text));
    if let Some(va) = state(ctx, |s| s.strings.get(&key).copied()) {
        return va;
    }
    let va = alloc(ctx, text.len() as u64 * 2 + 2);
    put_w(memory, va, text);
    state(ctx, |s| s.strings.insert(key, va));
    va
}

/// A NUL-terminated string of `unit`-byte characters at a 64-bit address,
/// read a page at a time so that neither its length nor the end of its
/// mapping cuts it short.
fn units_until_nul(memory: &mut dyn GuestMemory, va: u64, unit: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    if va == 0 {
        return bytes;
    }
    let mut at = va;
    while bytes.len() < 1 << 26 {
        let length = (0x1000 - (at & 0xFFF)) as usize;
        let length = if length < unit { unit } else { length - length % unit };
        // A mapping smaller than a page: fall back to short reads.
        let Some(chunk) = [length, 64.min(length), unit].into_iter().find_map(|n| memory.read_bytes_at(at, n)) else {
            break;
        };
        let length = chunk.len();
        if let Some(end) = chunk.chunks_exact(unit).position(|c| c.iter().all(|&b| b == 0)) {
            bytes.extend_from_slice(&chunk[..end * unit]);
            return bytes;
        }
        bytes.extend_from_slice(&chunk);
        at += length as u64;
    }
    bytes
}

fn cstr(memory: &mut dyn GuestMemory, va: u64) -> Vec<u8> {
    units_until_nul(memory, va, 1)
}

fn wstr(memory: &mut dyn GuestMemory, va: u64) -> Vec<u16> {
    units_until_nul(memory, va, 2).chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

fn put_c(memory: &mut dyn GuestMemory, va: u32, text: &[u8]) {
    let mut bytes = text.to_vec();
    bytes.push(0);
    let _ = memory.write_bytes(va, &bytes);
}

fn put_w(memory: &mut dyn GuestMemory, va: u32, text: &[u16]) {
    unicode::write_wide(memory, va, text, true);
}

fn units_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

fn read_units(memory: &mut dyn GuestMemory, va: u64, count: usize, wide: bool) -> Vec<u16> {
    if wide {
        let bytes = memory.read_bytes_at(va, count * 2).unwrap_or_default();
        bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
    } else {
        memory.read_bytes_at(va, count).unwrap_or_default().into_iter().map(u16::from).collect()
    }
}

fn f64_arg(memory: &mut dyn GuestMemory, index: usize) -> f64 {
    f64::from_bits(memory.xmm(index))
}

fn f32_arg(memory: &mut dyn GuestMemory, index: usize) -> f32 {
    f32::from_bits(memory.xmm(index) as u32)
}

// ── Character classes of the "C" locale ──────────────────────────────────

const UPPER: u16 = 0x1;
const LOWER: u16 = 0x2;
const DIGIT: u16 = 0x4;
const SPACE: u16 = 0x8;
const PUNCT: u16 = 0x10;
const CONTROL: u16 = 0x20;
const BLANK: u16 = 0x40;
const HEX: u16 = 0x80;
const ALPHA: u16 = 0x100;

fn ctype(c: u32) -> u16 {
    let Ok(b) = u8::try_from(c) else { return 0 };
    let mut flags = 0;
    if b.is_ascii_uppercase() {
        flags |= UPPER | ALPHA;
    }
    if b.is_ascii_lowercase() {
        flags |= LOWER | ALPHA;
    }
    if b.is_ascii_digit() {
        flags |= DIGIT;
    }
    if b.is_ascii_hexdigit() {
        flags |= HEX;
    }
    if matches!(b, b' ' | b'\t'..=b'\r') {
        flags |= SPACE;
    }
    if matches!(b, b' ' | b'\t') {
        flags |= BLANK;
    }
    if b < 0x20 || b == 0x7F {
        flags |= CONTROL;
    }
    if b.is_ascii_punctuation() {
        flags |= PUNCT;
    }
    flags
}

fn lower_unit(unit: u16) -> u16 {
    char::from_u32(unit as u32)
        .map(|c| {
            let mut l = c.to_lowercase();
            if l.len() == 1 { l.next().unwrap_or(c) as u32 as u16 } else { unit }
        })
        .unwrap_or(unit)
}

fn upper_unit(unit: u16) -> u16 {
    char::from_u32(unit as u32)
        .map(|c| {
            let mut u = c.to_uppercase();
            if u.len() == 1 { u.next().unwrap_or(c) as u32 as u16 } else { unit }
        })
        .unwrap_or(unit)
}

// ── Number parsing ───────────────────────────────────────────────────────

/// Parse an integer as strtol does: (magnitude, negative, consumed,
/// overflowed past u64).
fn parse_integer(text: &[u16], base: u32) -> (u64, bool, usize, bool) {
    let mut i = 0;
    while i < text.len() && is_space(text[i]) {
        i += 1;
    }
    let mut negative = false;
    if i < text.len() && (text[i] == b'-' as u16 || text[i] == b'+' as u16) {
        negative = text[i] == b'-' as u16;
        i += 1;
    }
    let mut base = base;
    let at = |k: usize| text.get(k).copied().unwrap_or(0);
    if (base == 0 || base == 16) && at(i) == b'0' as u16 && (at(i + 1) | 0x20) == b'x' as u16 && digit(at(i + 2), 16).is_some() {
        i += 2;
        base = 16;
    } else if base == 0 && at(i) == b'0' as u16 {
        base = 8;
    } else if base == 0 {
        base = 10;
    }
    if !(2..=36).contains(&base) {
        return (0, false, 0, false);
    }
    let start = i;
    let mut value: u64 = 0;
    let mut overflow = false;
    while let Some(d) = digit(at(i), base) {
        match value.checked_mul(base as u64).and_then(|v| v.checked_add(d as u64)) {
            Some(v) => value = v,
            None => overflow = true,
        }
        i += 1;
    }
    if i == start {
        return (0, false, 0, false);
    }
    (value, negative, i, overflow)
}

fn digit(unit: u16, base: u32) -> Option<u32> {
    char::from_u32(unit as u32)?.to_digit(base)
}

fn is_space(unit: u16) -> bool {
    matches!(unit, 0x20 | 0x09..=0x0D)
}

/// Parse a floating-point number as strtod does: (value, consumed).
fn parse_float(text: &[u16]) -> (f64, usize) {
    let mut i = 0;
    while i < text.len() && is_space(text[i]) {
        i += 1;
    }
    let start = i;
    let mut s = String::new();
    let at = |k: usize| text.get(k).map(|&u| (u as u8 as char).to_ascii_lowercase()).unwrap_or('\0');
    if at(i) == '+' || at(i) == '-' {
        s.push(at(i));
        i += 1;
    }
    let word = |i: usize, w: &str| w.chars().enumerate().all(|(k, c)| at(i + k) == c);
    if word(i, "infinity") {
        return (if s == "-" { f64::NEG_INFINITY } else { f64::INFINITY }, i + 8);
    }
    if word(i, "inf") {
        return (if s == "-" { f64::NEG_INFINITY } else { f64::INFINITY }, i + 3);
    }
    if word(i, "nan") {
        return (f64::NAN, i + 3);
    }
    let mut digits = 0;
    while at(i).is_ascii_digit() {
        s.push(at(i));
        i += 1;
        digits += 1;
    }
    if at(i) == '.' {
        s.push('.');
        i += 1;
        while at(i).is_ascii_digit() {
            s.push(at(i));
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return (0.0, 0);
    }
    if at(i) == 'e' {
        let mut j = i + 1;
        let mut exponent = String::from("e");
        if at(j) == '+' || at(j) == '-' {
            exponent.push(at(j));
            j += 1;
        }
        if at(j).is_ascii_digit() {
            while at(j).is_ascii_digit() {
                exponent.push(at(j));
                j += 1;
            }
            s.push_str(&exponent);
            i = j;
        }
    }
    let _ = start;
    (s.parse().unwrap_or(0.0), i)
}

// ── Formatted output ─────────────────────────────────────────────────────

/// Pointer-sized slots of a `va_list`.
struct VaList {
    at: u32,
    slot: u32,
}

impl VaList {
    fn next(&mut self, memory: &mut dyn GuestMemory, size: u32) -> u64 {
        let value = if size == 8 || self.slot == 8 {
            memory.read_u64(self.at).unwrap_or(0)
        } else {
            memory.read_u32(self.at).unwrap_or(0) as u64
        };
        self.at += size.max(self.slot);
        value
    }
}

fn pad(out: &mut Vec<u16>, body: &[u16], width: usize, left: bool, zero_after: Option<usize>) {
    let fill = width.saturating_sub(body.len());
    if left {
        out.extend_from_slice(body);
        out.extend(std::iter::repeat_n(b' ' as u16, fill));
    } else if let Some(prefix) = zero_after {
        out.extend_from_slice(&body[..prefix]);
        out.extend(std::iter::repeat_n(b'0' as u16, fill));
        out.extend_from_slice(&body[prefix..]);
    } else {
        out.extend(std::iter::repeat_n(b' ' as u16, fill));
        out.extend_from_slice(body);
    }
}

fn ascii(text: &str) -> Vec<u16> {
    text.bytes().map(u16::from).collect()
}

/// Format a floating-point number for %e, %f, %g, %a.
fn format_float(value: f64, conversion: u8, precision: Option<usize>, alternate: bool) -> String {
    let upper = conversion.is_ascii_uppercase();
    let lowered = conversion.to_ascii_lowercase();
    if value.is_nan() {
        let text = if value.is_sign_negative() { "-nan(ind)" } else { "nan" };
        return if upper { text.to_uppercase() } else { text.into() };
    }
    if value.is_infinite() {
        let text = if value < 0.0 { "-inf" } else { "inf" };
        return if upper { text.to_uppercase() } else { text.into() };
    }
    let exp_form = |v: f64, p: usize| {
        let text = format!("{:.*e}", p, v);
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        let exponent: i32 = exponent.parse().unwrap_or(0);
        let mut mantissa = mantissa.to_string();
        if alternate && p == 0 {
            mantissa.push('.');
        }
        format!("{mantissa}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
    };
    let text = match lowered {
        b'f' => {
            let mut text = format!("{:.*}", precision.unwrap_or(6), value);
            if alternate && precision == Some(0) {
                text.push('.');
            }
            text
        }
        b'e' => exp_form(value, precision.unwrap_or(6)),
        b'a' => {
            // Hexadecimal: 0x1.hhhp+d.
            let bits = value.to_bits();
            let negative = bits >> 63 != 0;
            let exponent = ((bits >> 52) & 0x7FF) as i32;
            let mantissa = bits & ((1 << 52) - 1);
            let (lead, exponent) = if exponent == 0 {
                (0, if mantissa == 0 { 0 } else { -1022 })
            } else {
                (1, exponent - 1023)
            };
            let mut hex = format!("{mantissa:013x}");
            match precision {
                Some(p) => hex.truncate(p),
                None => {
                    while hex.ends_with('0') {
                        hex.pop();
                    }
                }
            }
            let dot = if hex.is_empty() && !alternate { "" } else { "." };
            format!("{}0x{lead}{dot}{hex}p{}{}", if negative { "-" } else { "" }, if exponent < 0 { '-' } else { '+' }, exponent.abs())
        }
        _ => {
            // %g: %e or %f by the exponent, trailing zeros removed.
            let p = match precision {
                Some(0) => 1,
                Some(p) => p,
                None => 6,
            };
            if value == 0.0 {
                let mut text = format!("{:.*}", if alternate { p - 1 } else { 0 }, value);
                if alternate && p == 1 {
                    text.push('.');
                }
                return text;
            }
            let probe = format!("{:.*e}", p - 1, value);
            let exponent: i32 = probe.split_once('e').map(|(_, e)| e.parse().unwrap_or(0)).unwrap_or(0);
            let mut text = if exponent < -4 || exponent >= p as i32 {
                exp_form(value, p - 1)
            } else {
                format!("{:.*}", (p as i32 - 1 - exponent).max(0) as usize, value)
            };
            if !alternate {
                let (number, suffix) = match text.find('e') {
                    Some(e) => (text[..e].to_string(), text[e..].to_string()),
                    None => (text.clone(), String::new()),
                };
                let mut number = number;
                if number.contains('.') {
                    while number.ends_with('0') {
                        number.pop();
                    }
                    if number.ends_with('.') {
                        number.pop();
                    }
                }
                text = number + &suffix;
            }
            text
        }
    };
    if upper { text.to_uppercase() } else { text }
}

/// Format `format` (code units; `wide` says whether the output is wide)
/// with arguments from `va`. `legacy_wide` makes %s and %c mean wide in a
/// wide format, as in the Microsoft C library by default.
fn format(memory: &mut dyn GuestMemory, fmt: &[u16], va: u32, wide: bool, legacy_wide: bool) -> Vec<u16> {
    let slot = memory.ptr_size();
    let mut args = VaList { at: va, slot };
    let mut out: Vec<u16> = Vec::new();
    let mut i = 0;
    let at = |k: usize| fmt.get(k).copied().unwrap_or(0);
    while i < fmt.len() {
        let unit = fmt[i];
        if unit != b'%' as u16 {
            out.push(unit);
            i += 1;
            continue;
        }
        i += 1;
        let (mut left, mut plus, mut space, mut alternate, mut zero) = (false, false, false, false, false);
        loop {
            match at(i) as u8 {
                b'-' => left = true,
                b'+' => plus = true,
                b' ' => space = true,
                b'#' => alternate = true,
                b'0' => zero = true,
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        if at(i) == b'*' as u16 {
            let w = args.next(memory, 4) as u32 as i32;
            if w < 0 {
                left = true;
            }
            width = w.unsigned_abs() as usize;
            i += 1;
        } else {
            while (at(i) as u8).is_ascii_digit() && at(i) < 0x80 {
                width = width * 10 + (at(i) - b'0' as u16) as usize;
                i += 1;
            }
        }
        let mut precision: Option<usize> = None;
        if at(i) == b'.' as u16 {
            i += 1;
            if at(i) == b'*' as u16 {
                let p = args.next(memory, 4) as u32 as i32;
                precision = if p < 0 { None } else { Some(p as usize) };
                i += 1;
            } else {
                let mut p = 0;
                while (at(i) as u8).is_ascii_digit() && at(i) < 0x80 {
                    p = p * 10 + (at(i) - b'0' as u16) as usize;
                    i += 1;
                }
                precision = Some(p);
            }
        }
        // Length: size in bytes of an integer argument; `w`ide/`h`narrow
        // for characters and strings.
        let mut size: u32 = 4;
        let mut char_width: Option<bool> = None;
        loop {
            match at(i) as u8 {
                b'h' if at(i) < 0x80 => {
                    if at(i + 1) == b'h' as u16 {
                        size = 1;
                        i += 1;
                    } else {
                        size = 2;
                    }
                    char_width = Some(false);
                }
                b'l' if at(i) < 0x80 => {
                    if at(i + 1) == b'l' as u16 {
                        size = 8;
                        i += 1;
                    } else {
                        size = 4;
                        char_width = Some(true);
                    }
                }
                b'w' => char_width = Some(true),
                b'L' | b'q' => size = 8,
                b'j' => size = 8,
                b'z' | b't' => size = slot,
                b'I' => {
                    if at(i + 1) == b'6' as u16 && at(i + 2) == b'4' as u16 {
                        size = 8;
                        i += 2;
                    } else if at(i + 1) == b'3' as u16 && at(i + 2) == b'2' as u16 {
                        size = 4;
                        i += 2;
                    } else {
                        size = slot;
                    }
                }
                _ => break,
            }
            i += 1;
        }
        let conversion = at(i) as u8;
        i += 1;
        let zero_ok = zero && !left;
        match conversion {
            b'%' => out.push(b'%' as u16),
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                let size = if conversion == b'p' { slot } else { size };
                let raw = args.next(memory, if size == 8 { 8 } else { 4 });
                let signed = matches!(conversion, b'd' | b'i');
                let (negative, magnitude) = if signed {
                    let v: i64 = match size {
                        1 => raw as i8 as i64,
                        2 => raw as i16 as i64,
                        8 => raw as i64,
                        _ => raw as u32 as i32 as i64,
                    };
                    (v < 0, v.unsigned_abs())
                } else {
                    let v: u64 = match size {
                        1 => raw as u8 as u64,
                        2 => raw as u16 as u64,
                        8 => raw,
                        _ => raw as u32 as u64,
                    };
                    (false, v)
                };
                let mut digits = match conversion {
                    b'o' => format!("{magnitude:o}"),
                    b'x' => format!("{magnitude:x}"),
                    b'X' => format!("{magnitude:X}"),
                    b'p' => format!("{:0width$X}", magnitude, width = slot as usize * 2),
                    _ => magnitude.to_string(),
                };
                if let Some(p) = precision {
                    if p == 0 && magnitude == 0 && conversion != b'p' {
                        digits.clear();
                    }
                    while digits.len() < p {
                        digits.insert(0, '0');
                    }
                }
                let mut prefix = String::new();
                if negative {
                    prefix.push('-');
                } else if signed && plus {
                    prefix.push('+');
                } else if signed && space {
                    prefix.push(' ');
                }
                if alternate && magnitude != 0 {
                    match conversion {
                        b'x' => prefix.push_str("0x"),
                        b'X' => prefix.push_str("0X"),
                        b'o' if !digits.starts_with('0') => prefix.push('0'),
                        _ => {}
                    }
                }
                let body = ascii(&(prefix.clone() + &digits));
                let zero_after = (zero_ok && precision.is_none()).then_some(prefix.len());
                pad(&mut out, &body, width, left, zero_after);
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let value = f64::from_bits(args.next(memory, 8));
                let mut text = format_float(value, conversion, precision, alternate);
                if !text.starts_with('-') {
                    if plus {
                        text.insert(0, '+');
                    } else if space {
                        text.insert(0, ' ');
                    }
                }
                let sign = usize::from(text.starts_with(['-', '+', ' ']));
                let finite = value.is_finite();
                pad(&mut out, &ascii(&text), width, left, (zero_ok && finite).then_some(sign));
            }
            b'c' | b'C' => {
                let value = args.next(memory, 4) as u32;
                let is_wide = match char_width {
                    Some(w) => w,
                    None => (conversion == b'C') != (wide && legacy_wide),
                };
                let unit = if is_wide { value as u16 } else { unicode::ansi_to_wide(&[value as u8]).first().copied().unwrap_or(0) };
                pad(&mut out, &[unit], width, left, None);
            }
            b's' | b'S' | b'Z' => {
                let pointer = args.next(memory, slot);
                let is_wide = match char_width {
                    Some(w) => w,
                    None => (conversion == b'S') != (wide && legacy_wide),
                };
                let mut text: Vec<u16> = if pointer == 0 {
                    ascii("(null)")
                } else if is_wide {
                    match precision {
                        Some(p) => {
                            let mut units = wstr(memory, pointer);
                            units.truncate(p);
                            units
                        }
                        None => wstr(memory, pointer),
                    }
                } else {
                    let mut bytes = cstr(memory, pointer);
                    if let Some(p) = precision {
                        bytes.truncate(p);
                    }
                    unicode::ansi_to_wide(&bytes)
                };
                if let Some(p) = precision {
                    text.truncate(p);
                }
                pad(&mut out, &text, width, left, None);
            }
            b'n' => {
                let pointer = args.next(memory, slot);
                if pointer != 0 {
                    let _ = memory.write_u32(pointer as u32, out.len() as u32);
                }
            }
            0 => break,
            other => out.push(other as u16),
        }
    }
    out
}

fn output_for(out: &[u16], wide: bool) -> Vec<u16> {
    if wide { out.to_vec() } else { unicode::wide_to_ansi(out).into_iter().map(u16::from).collect() }
}

fn write_units(memory: &mut dyn GuestMemory, va: u32, units: &[u16], wide: bool, terminate: bool) {
    if wide {
        let mut bytes = units_bytes(units);
        if terminate {
            bytes.extend_from_slice(&[0, 0]);
        }
        let _ = memory.write_bytes(va, &bytes);
    } else {
        let mut bytes: Vec<u8> = units.iter().map(|&u| u as u8).collect();
        if terminate {
            bytes.push(0);
        }
        let _ = memory.write_bytes(va, &bytes);
    }
}

fn read_format(memory: &mut dyn GuestMemory, va: u64, wide: bool) -> Vec<u16> {
    if wide { wstr(memory, va) } else { cstr(memory, va).into_iter().map(u16::from).collect() }
}

// ── Formatted input ──────────────────────────────────────────────────────

/// Scan `input` by `fmt`, storing through pointers from `va`. Returns
/// (fields assigned, units consumed), or None at end of input before any.
fn scan(memory: &mut dyn GuestMemory, input: &[u16], fmt: &[u16], va: u32, wide: bool) -> (i32, usize) {
    let slot = memory.ptr_size();
    let mut args = VaList { at: va, slot };
    let (mut i, mut k, mut assigned) = (0usize, 0usize, 0i32);
    let at = |k: usize| fmt.get(k).copied().unwrap_or(0);
    let mut any_input = false;
    while i < fmt.len() {
        let unit = fmt[i];
        if is_space(unit) {
            while k < input.len() && is_space(input[k]) {
                k += 1;
            }
            i += 1;
            continue;
        }
        if unit != b'%' as u16 || at(i + 1) == b'%' as u16 {
            let expected = if unit == b'%' as u16 { i += 1; b'%' as u16 } else { unit };
            if input.get(k) != Some(&expected) {
                break;
            }
            k += 1;
            i += 1;
            continue;
        }
        i += 1;
        let suppress = at(i) == b'*' as u16;
        if suppress {
            i += 1;
        }
        let mut width = 0usize;
        while (b'0' as u16..=b'9' as u16).contains(&at(i)) {
            width = width * 10 + (at(i) - b'0' as u16) as usize;
            i += 1;
        }
        let mut size = 4u32;
        let mut long = false;
        loop {
            match at(i) as u8 {
                b'h' => size = if at(i + 1) == b'h' as u16 { i += 1; 1 } else { 2 },
                b'l' => {
                    if at(i + 1) == b'l' as u16 {
                        i += 1;
                        size = 8;
                    } else {
                        long = true;
                    }
                }
                b'L' | b'j' => size = 8,
                b'z' | b't' => size = slot,
                b'I' => {
                    if at(i + 1) == b'6' as u16 {
                        i += 2;
                        size = 8;
                    } else {
                        size = slot;
                    }
                }
                b'w' => long = true,
                _ => break,
            }
            i += 1;
        }
        let conversion = at(i) as u8;
        i += 1;
        if conversion != b'c' && conversion != b'[' && conversion != b'n' {
            while k < input.len() && is_space(input[k]) {
                k += 1;
            }
        }
        if k >= input.len() && conversion != b'n' {
            if !any_input {
                return (-1, k);
            }
            break;
        }
        let limit = if width == 0 { usize::MAX } else { width };
        let rest = &input[k..input.len().min(k.saturating_add(limit))];
        match conversion {
            b'd' | b'i' | b'u' | b'x' | b'X' | b'o' | b'p' => {
                let base = match conversion {
                    b'd' | b'u' => 10,
                    b'x' | b'X' | b'p' => 16,
                    b'o' => 8,
                    _ => 0,
                };
                let (value, negative, used, _) = parse_integer(rest, base);
                if used == 0 {
                    break;
                }
                k += used;
                any_input = true;
                if !suppress {
                    let value = if negative { value.wrapping_neg() } else { value };
                    let target = args.next(memory, slot) as u32;
                    let size = if conversion == b'p' { slot } else { size };
                    let bytes = value.to_le_bytes();
                    let _ = memory.write_bytes(target, &bytes[..size as usize]);
                    assigned += 1;
                }
            }
            b'e' | b'E' | b'f' | b'g' | b'G' | b'a' => {
                let (value, used) = parse_float(rest);
                if used == 0 {
                    break;
                }
                k += used;
                any_input = true;
                if !suppress {
                    let target = args.next(memory, slot) as u32;
                    if long || size == 8 {
                        let _ = memory.write_u64(target, value.to_bits());
                    } else {
                        let _ = memory.write_u32(target, (value as f32).to_bits());
                    }
                    assigned += 1;
                }
            }
            b's' | b'c' | b'[' => {
                let mut set: Vec<u16> = Vec::new();
                let mut negate = false;
                if conversion == b'[' {
                    if at(i) == b'^' as u16 {
                        negate = true;
                        i += 1;
                    }
                    if at(i) == b']' as u16 {
                        set.push(b']' as u16);
                        i += 1;
                    }
                    while i < fmt.len() && at(i) != b']' as u16 {
                        if at(i + 1) == b'-' as u16 && at(i + 2) != b']' as u16 && at(i + 2) != 0 {
                            for c in at(i)..=at(i + 2) {
                                set.push(c);
                            }
                            i += 3;
                        } else {
                            set.push(at(i));
                            i += 1;
                        }
                    }
                    i += 1;
                }
                let count = match conversion {
                    b'c' => rest.len().min(if width == 0 { 1 } else { width }),
                    b's' => rest.iter().position(|&u| is_space(u)).unwrap_or(rest.len()),
                    _ => rest.iter().position(|u| set.contains(u) == negate).unwrap_or(rest.len()),
                };
                if count == 0 {
                    break;
                }
                let taken = &rest[..count];
                k += count;
                any_input = true;
                if !suppress {
                    let target = args.next(memory, slot) as u32;
                    // The secure forms pass a buffer size after each buffer.
                    let is_wide = long || (wide && !matches!(size, 1 | 2));
                    write_units(memory, target, &output_for(taken, is_wide), is_wide, conversion != b'c');
                    assigned += 1;
                }
            }
            b'n' => {
                if !suppress {
                    let target = args.next(memory, slot) as u32;
                    let _ = memory.write_u32(target, k as u32);
                }
            }
            _ => break,
        }
    }
    (assigned, k)
}

// ── Time ─────────────────────────────────────────────────────────────────

fn now() -> (i64, u32) {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (since.as_secs() as i64, since.subsec_millis())
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// struct tm fields for a Unix time (UTC): sec, min, hour, mday, mon,
/// year-1900, wday, yday, isdst.
fn broken_down(secs: i64) -> [i32; 9] {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let yday = days - days_from_civil(year, 1, 1);
    [
        (rem % 60) as i32,
        (rem / 60 % 60) as i32,
        (rem / 3600) as i32,
        d as i32,
        (m - 1) as i32,
        (year - 1900) as i32,
        (days + 4).rem_euclid(7) as i32,
        yday as i32,
        0,
    ]
}

fn read_tm(memory: &mut dyn GuestMemory, va: u32) -> [i32; 9] {
    let mut tm = [0i32; 9];
    for (index, field) in tm.iter_mut().enumerate() {
        *field = memory.read_u32(va + index as u32 * 4).unwrap_or(0) as i32;
    }
    tm
}

fn write_tm(memory: &mut dyn GuestMemory, va: u32, tm: &[i32; 9]) {
    let bytes: Vec<u8> = tm.iter().flat_map(|v| v.to_le_bytes()).collect();
    let _ = memory.write_bytes(va, &bytes);
}

fn time_of(tm: &[i32; 9]) -> i64 {
    let month = tm[4] as i64;
    let year = tm[5] as i64 + 1900 + month.div_euclid(12);
    let month = month.rem_euclid(12) + 1;
    days_from_civil(year, month, 1) * 86400
        + (tm[3] as i64 - 1) * 86400
        + tm[2] as i64 * 3600
        + tm[1] as i64 * 60
        + tm[0] as i64
}

const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn strftime(fmt: &[u16], tm: &[i32; 9]) -> Vec<u16> {
    let mut out = String::new();
    let mut i = 0;
    let day = DAYS[tm[6].rem_euclid(7) as usize];
    let month = MONTHS[tm[4].rem_euclid(12) as usize];
    let year = tm[5] + 1900;
    let hour12 = if tm[2] % 12 == 0 { 12 } else { tm[2] % 12 };
    while i < fmt.len() {
        let unit = fmt[i];
        i += 1;
        if unit != b'%' as u16 {
            out.push(char::from_u32(unit as u32).unwrap_or('?'));
            continue;
        }
        if fmt.get(i) == Some(&(b'#' as u16)) {
            i += 1;
        }
        let Some(&code) = fmt.get(i) else { break };
        i += 1;
        let text = match code as u8 {
            b'a' => day[..3].to_string(),
            b'A' => day.to_string(),
            b'b' | b'h' => month[..3].to_string(),
            b'B' => month.to_string(),
            b'c' => format!("{:02}/{:02}/{:02} {:02}:{:02}:{:02}", tm[4] + 1, tm[3], year % 100, tm[2], tm[1], tm[0]),
            b'C' => format!("{:02}", year / 100),
            b'd' => format!("{:02}", tm[3]),
            b'D' | b'x' => format!("{:02}/{:02}/{:02}", tm[4] + 1, tm[3], year % 100),
            b'e' => format!("{:2}", tm[3]),
            b'F' => format!("{year:04}-{:02}-{:02}", tm[4] + 1, tm[3]),
            b'H' => format!("{:02}", tm[2]),
            b'I' => format!("{hour12:02}"),
            b'j' => format!("{:03}", tm[7] + 1),
            b'm' => format!("{:02}", tm[4] + 1),
            b'M' => format!("{:02}", tm[1]),
            b'n' => "\n".into(),
            b'p' => (if tm[2] < 12 { "AM" } else { "PM" }).into(),
            b'r' => format!("{hour12:02}:{:02}:{:02} {}", tm[1], tm[0], if tm[2] < 12 { "AM" } else { "PM" }),
            b'R' => format!("{:02}:{:02}", tm[2], tm[1]),
            b'S' => format!("{:02}", tm[0]),
            b't' => "\t".into(),
            b'T' | b'X' => format!("{:02}:{:02}:{:02}", tm[2], tm[1], tm[0]),
            b'u' => format!("{}", if tm[6] == 0 { 7 } else { tm[6] }),
            b'w' => format!("{}", tm[6]),
            b'U' => format!("{:02}", (tm[7] + 7 - tm[6]) / 7),
            b'W' => format!("{:02}", (tm[7] + 7 - (tm[6] + 6) % 7) / 7),
            b'y' => format!("{:02}", year % 100),
            b'Y' => format!("{year}"),
            b'z' => "+0000".into(),
            b'Z' => "UTC".into(),
            b'%' => "%".into(),
            _ => String::new(),
        };
        out.push_str(&text);
    }
    out.encode_utf16().collect()
}

// ── Files ────────────────────────────────────────────────────────────────

fn fd_of(ctx: &ApiContext, fd: i32) -> Option<Fd> {
    match fd {
        0..=2 => Some(state(ctx, |s| s.fds.get(&fd).copied()).unwrap_or(Fd::Std(fd as u32))),
        _ => state(ctx, |s| s.fds.get(&fd).copied()),
    }
}

fn new_fd(ctx: &ApiContext, fd: Fd) -> i32 {
    state(ctx, |s| {
        let mut n = 3;
        while s.fds.contains_key(&n) {
            n += 1;
        }
        s.fds.insert(n, fd);
        n
    })
}

fn fd_write(ctx: &ApiContext, fd: i32, bytes: &[u8]) -> Option<usize> {
    match fd_of(ctx, fd)? {
        Fd::Std(1) => {
            let mut out = std::io::stdout();
            out.write_all(bytes).ok()?;
            let _ = out.flush();
            Some(bytes.len())
        }
        Fd::Std(2) => {
            std::io::stderr().write_all(bytes).ok()?;
            Some(bytes.len())
        }
        Fd::Std(_) => None,
        Fd::File(handle) => crt::write_file(ctx, handle, bytes),
    }
}

fn fd_read(ctx: &ApiContext, fd: i32, length: usize) -> Option<Vec<u8>> {
    match fd_of(ctx, fd)? {
        Fd::Std(_) => Some(Vec::new()),
        Fd::File(handle) => crt::read_file(ctx, handle, length),
    }
}

/// Open a file by C library flags (`_O_*`).
fn open_path(ctx: &ApiContext, path: &[u8], flags: u32) -> Result<i32, u32> {
    let access = flags & 3;
    let (read, write) = (access != 1, access != 0);
    let create = flags & 0x100 != 0;
    let truncate = flags & 0x200 != 0;
    let exclusive = flags & 0x400 != 0;
    match crt::open_file(ctx, path, read, write, create, truncate, exclusive && create) {
        Ok(handle) => {
            if flags & 8 != 0 {
                crt::seek_file(ctx, handle, 0, 2);
            }
            Ok(new_fd(ctx, Fd::File(handle)))
        }
        Err(80) => Err(17), // EEXIST
        Err(2 | 3) => Err(ENOENT),
        Err(5) => Err(13), // EACCES
        Err(_) => Err(EINVAL),
    }
}

/// C library flags for an `fopen` mode.
fn mode_flags(mode: &[u8]) -> Option<u32> {
    let mut flags = match mode.first()? {
        b'r' => 0,
        b'w' => 1 | 0x100 | 0x200,
        b'a' => 1 | 0x100 | 8,
        _ => return None,
    };
    if mode.contains(&b'+') {
        flags = (flags & !3) | 2;
    }
    Some(flags)
}

fn new_stream(ctx: &ApiContext, memory: &mut dyn GuestMemory, fd: i32) -> u32 {
    let file = alloc(ctx, 0x60);
    let _ = memory.write_bytes(file, &[0u8; 0x60]);
    // _file at the C library's offset, for code that peeks.
    let _ = memory.write_u32(file + if memory.ptr_size() == 8 { 0x18 } else { 0x10 }, fd as u32);
    state(ctx, |s| s.streams.insert(file, Stream { fd, ..Stream::default() }));
    file
}

fn standard_stream(ctx: &ApiContext, memory: &mut dyn GuestMemory, index: u32) -> u32 {
    let key = format!("iob{index}");
    if let Some(va) = state(ctx, |s| s.statics.get(&key).copied()) {
        return va;
    }
    let file = new_stream(ctx, memory, index as i32);
    state(ctx, |s| s.statics.insert(key, file));
    file
}

fn stream(ctx: &ApiContext, file: u64) -> Option<Stream> {
    state(ctx, |s| s.streams.get(&(file as u32)).cloned())
}

fn update_stream(ctx: &ApiContext, file: u64, f: impl FnOnce(&mut Stream)) {
    state(ctx, |s| {
        if let Some(stream) = s.streams.get_mut(&(file as u32)) {
            f(stream);
        }
    });
}

fn stream_write(ctx: &ApiContext, file: u64, bytes: &[u8]) -> Option<usize> {
    let stream = stream(ctx, file)?;
    let written = fd_write(ctx, stream.fd, bytes);
    if written.is_none() {
        update_stream(ctx, file, |s| s.error = true);
    }
    written
}

fn stream_read(ctx: &ApiContext, file: u64, length: usize) -> Vec<u8> {
    let Some(stream) = stream(ctx, file) else { return Vec::new() };
    let mut out: Vec<u8> = Vec::new();
    let mut pushed = stream.pushed.clone();
    while out.len() < length {
        match pushed.pop() {
            Some(b) => out.push(b),
            None => break,
        }
    }
    if out.len() < length {
        let more = fd_read(ctx, stream.fd, length - out.len()).unwrap_or_default();
        out.extend_from_slice(&more);
    }
    let eof = out.len() < length;
    update_stream(ctx, file, |s| {
        s.pushed = pushed;
        if eof {
            s.eof = true;
        }
    });
    out
}

fn stream_seek(ctx: &ApiContext, file: u64, offset: i64, whence: u32) -> Option<u64> {
    let stream = stream(ctx, file)?;
    let adjust = if whence == 1 { -(stream.pushed.len() as i64) } else { 0 };
    let Fd::File(handle) = fd_of(ctx, stream.fd)? else { return None };
    let position = crt::seek_file(ctx, handle, offset + adjust, whence)?;
    update_stream(ctx, file, |s| {
        s.pushed.clear();
        s.eof = false;
    });
    Some(position)
}

fn stream_tell(ctx: &ApiContext, file: u64) -> Option<u64> {
    let stream = stream(ctx, file)?;
    let Fd::File(handle) = fd_of(ctx, stream.fd)? else { return Some(0) };
    let position = crt::seek_file(ctx, handle, 0, 1)?;
    Some(position.saturating_sub(stream.pushed.len() as u64))
}

fn close_fd(ctx: &ApiContext, fd: i32) -> bool {
    match state(ctx, |s| s.fds.remove(&fd)) {
        Some(Fd::File(handle)) => crt::close_file(ctx, handle),
        Some(Fd::Std(_)) => true,
        None => (0..=2).contains(&fd),
    }
}

fn path_arg(memory: &mut dyn GuestMemory, va: u64, wide: bool) -> Vec<u8> {
    if wide { unicode::wide_to_ansi(&wstr(memory, va)) } else { cstr(memory, va) }
}

/// Fill a `struct _stat64` (or `_stat64i32`) for a host path.
fn write_stat(memory: &mut dyn GuestMemory, va: u32, metadata: &std::fs::Metadata, small: bool) {
    let directory = metadata.is_dir();
    let mut mode: u16 = if directory { 0x4000 | 0x49 } else { 0x8000 };
    mode |= 0x124; // readable
    if !metadata.permissions().readonly() {
        mode |= 0x92;
    }
    let seconds = |time: std::io::Result<std::time::SystemTime>| {
        time.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
    };
    let (atime, mtime, ctime) = (seconds(metadata.accessed()), seconds(metadata.modified()), seconds(metadata.modified()));
    let mut bytes = vec![0u8; if small { 48 } else { 56 }];
    bytes[0..4].copy_from_slice(&2u32.to_le_bytes()); // st_dev: drive C:
    bytes[6..8].copy_from_slice(&mode.to_le_bytes());
    bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
    bytes[16..20].copy_from_slice(&2u32.to_le_bytes());
    let size = if directory { 0 } else { metadata.len() };
    let times_at = if small {
        bytes[20..24].copy_from_slice(&(size.min(i32::MAX as u64) as u32).to_le_bytes());
        24
    } else {
        bytes[24..32].copy_from_slice(&size.to_le_bytes());
        32
    };
    for (index, time) in [atime, mtime, ctime].into_iter().enumerate() {
        bytes[times_at + index * 8..times_at + index * 8 + 8].copy_from_slice(&time.to_le_bytes());
    }
    let _ = memory.write_bytes(va, &bytes);
}

// ── Strings for the "C" locale's time names ──────────────────────────────

fn time_names() -> String {
    let mut text = String::new();
    for day in DAYS {
        text.push_str(&format!(":{}:{day}", &day[..3]));
    }
    text
}

fn month_names() -> String {
    let mut text = String::new();
    for month in MONTHS {
        text.push_str(&format!(":{}:{month}", &month[..3]));
    }
    text
}

const ERRORS: &[&str] = &[
    "No error", "Operation not permitted", "No such file or directory", "No such process", "Interrupted function call",
    "Input/output error", "No such device or address", "Arg list too long", "Exec format error", "Bad file descriptor",
    "No child processes", "Resource temporarily unavailable", "Not enough space", "Permission denied", "Bad address",
    "Unknown error", "Resource device", "File exists", "Improper link", "No such device", "Not a directory",
    "Is a directory", "Invalid argument", "Too many open files in system", "Too many open files",
    "Inappropriate I/O control operation", "Unknown error", "File too large", "No space left on device",
    "Invalid seek", "Read-only file system", "Too many links", "Broken pipe", "Domain error", "Result too large",
];

fn error_text(code: u32) -> &'static str {
    ERRORS.get(code as usize).copied().unwrap_or("Unknown error")
}

// ── Program arguments ────────────────────────────────────────────────────

/// Split a command line as the C library does.
pub fn split_command_line(line: &[u16]) -> Vec<Vec<u16>> {
    let mut args = Vec::new();
    let mut i = 0;
    let quote = b'"' as u16;
    let backslash = b'\\' as u16;
    loop {
        while i < line.len() && (line[i] == b' ' as u16 || line[i] == b'\t' as u16) {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        let mut arg = Vec::new();
        let mut quoted = false;
        while i < line.len() {
            let c = line[i];
            if (c == b' ' as u16 || c == b'\t' as u16) && !quoted {
                break;
            }
            if c == backslash {
                let mut n = 0;
                while i < line.len() && line[i] == backslash {
                    n += 1;
                    i += 1;
                }
                if i < line.len() && line[i] == quote {
                    arg.extend(std::iter::repeat_n(backslash, n / 2));
                    if n % 2 == 1 {
                        arg.push(quote);
                        i += 1;
                    }
                } else {
                    arg.extend(std::iter::repeat_n(backslash, n));
                }
                continue;
            }
            if c == quote {
                if quoted && line.get(i + 1) == Some(&quote) {
                    arg.push(quote);
                    i += 2;
                    continue;
                }
                quoted = !quoted;
                i += 1;
                continue;
            }
            arg.push(c);
            i += 1;
        }
        args.push(arg);
    }
    args
}

fn command_line(ctx: &ApiContext, memory: &mut dyn GuestMemory) -> Vec<u16> {
    unicode::ansi_to_wide(&read_cstr(memory, ctx.command_line_va))
}

/// argv (and argc) in guest memory, wide or narrow.
fn argv(ctx: &ApiContext, memory: &mut dyn GuestMemory, wide: bool) -> (u32, u32) {
    let key = if wide { "wargv" } else { "argv" };
    if let Some(va) = state(ctx, |s| s.statics.get(key).copied()) {
        let count = state(ctx, |s| s.statics.get("argc").copied()).unwrap_or(0);
        return (va, count);
    }
    let line = command_line(ctx, memory);
    let args = split_command_line(&line);
    let size = memory.ptr_size();
    let table = alloc(ctx, (args.len() as u64 + 1) * size as u64);
    for (index, arg) in args.iter().enumerate() {
        let text = if wide {
            let va = alloc(ctx, arg.len() as u64 * 2 + 2);
            put_w(memory, va, arg);
            va
        } else {
            let bytes = unicode::wide_to_ansi(arg);
            let va = alloc(ctx, bytes.len() as u64 + 1);
            put_c(memory, va, &bytes);
            va
        };
        let _ = memory.write_ptr(table + index as u32 * size, text as u64);
    }
    let _ = memory.write_ptr(table + args.len() as u32 * size, 0);
    state(ctx, |s| {
        s.statics.insert(key.into(), table);
        s.statics.insert("argc".into(), args.len() as u32);
    });
    (table, args.len() as u32)
}

fn environment(ctx: &ApiContext, memory: &mut dyn GuestMemory, wide: bool) -> u32 {
    let key = if wide { "wenviron" } else { "environ" };
    let size = memory.ptr_size();
    static_block(ctx, memory, key, size * 2)
}

// ── Calls ────────────────────────────────────────────────────────────────

/// Call guest code with pointer-sized arguments.
fn guest(memory: &mut dyn GuestMemory, function: u64, args: &[u64]) -> u64 {
    memory.call_guest_wide(function as u32, args).unwrap_or(0)
}

fn run_atexit(ctx: &ApiContext, memory: &mut dyn GuestMemory) {
    while let Some(function) = state(ctx, |s| s.atexit.pop()) {
        guest(memory, function, &[]);
    }
}

fn execute_table(memory: &mut dyn GuestMemory, table: u32) {
    let size = memory.ptr_size();
    let first = memory.read_ptr(table).unwrap_or(0) as u32;
    let last = memory.read_ptr(table + size).unwrap_or(0) as u32;
    if first == 0 || last < first {
        return;
    }
    let mut at = last;
    while at > first {
        at -= size;
        let function = memory.read_ptr(at).unwrap_or(0);
        if function != 0 {
            guest(memory, function, &[]);
        }
    }
    let _ = memory.write_ptr(table + size, first as u64);
}

fn register_in_table(ctx: &ApiContext, memory: &mut dyn GuestMemory, table: u32, function: u64) {
    let size = memory.ptr_size();
    let mut first = memory.read_ptr(table).unwrap_or(0) as u32;
    let mut last = memory.read_ptr(table + size).unwrap_or(0) as u32;
    let mut end = memory.read_ptr(table + 2 * size).unwrap_or(0) as u32;
    if first == 0 || last >= end {
        let count = if first == 0 { 0 } else { (last - first) / size };
        let capacity = (count * 2).max(32);
        let block = alloc(ctx, (capacity * size) as u64);
        if first != 0 {
            let old = memory.read_bytes(first, (count * size) as usize).unwrap_or_default();
            let _ = memory.write_bytes(block, &old);
        }
        first = block;
        last = block + count * size;
        end = block + capacity * size;
    }
    let _ = memory.write_ptr(last, function);
    let _ = memory.write_ptr(table, first as u64);
    let _ = memory.write_ptr(table + size, (last + size) as u64);
    let _ = memory.write_ptr(table + 2 * size, end as u64);
}

/// Sort `count` elements of `width` bytes at `base` with a guest
/// comparison function.
fn sort(ctx: &ApiContext, memory: &mut dyn GuestMemory, base: u32, count: usize, width: usize, compare: u64) {
    if count < 2 || width == 0 {
        return;
    }
    let mut items: Vec<Vec<u8>> =
        (0..count).map(|i| memory.read_bytes(base + (i * width) as u32, width).unwrap_or_default()).collect();
    let (a, b) = (alloc(ctx, width as u64), alloc(ctx, width as u64));
    // Merge sort: stable and O(n log n) comparisons.
    let cmp = |memory: &mut dyn GuestMemory, x: &[u8], y: &[u8]| {
        let _ = memory.write_bytes(a, x);
        let _ = memory.write_bytes(b, y);
        guest(memory, compare, &[a as u64, b as u64]) as u32 as i32
    };
    let mut width_run = 1;
    while width_run < items.len() {
        let mut merged = Vec::with_capacity(items.len());
        let mut start = 0;
        while start < items.len() {
            let middle = (start + width_run).min(items.len());
            let end = (start + 2 * width_run).min(items.len());
            let (mut i, mut j) = (start, middle);
            while i < middle && j < end {
                if cmp(memory, &items[j], &items[i]) < 0 {
                    merged.push(items[j].clone());
                    j += 1;
                } else {
                    merged.push(items[i].clone());
                    i += 1;
                }
            }
            merged.extend_from_slice(&items[i..middle]);
            merged.extend_from_slice(&items[j..end]);
            start = end;
        }
        items = merged;
        width_run *= 2;
    }
    let bytes: Vec<u8> = items.concat();
    let _ = memory.write_bytes(base, &bytes);
    release(ctx, a);
    release(ctx, b);
}

/// Copy at most `capacity` units of `text` plus a terminator; the secure
/// copy rules (`*_s`): 0, or ERANGE with an emptied destination.
fn secure_copy(memory: &mut dyn GuestMemory, dest: u32, capacity: u64, text: &[u16], wide: bool, truncate: bool) -> u32 {
    if dest == 0 || capacity == 0 {
        return EINVAL;
    }
    if (text.len() as u64) < capacity {
        write_units(memory, dest, text, wide, true);
        return 0;
    }
    if truncate {
        write_units(memory, dest, &text[..capacity as usize - 1], wide, true);
        return STRUNCATE;
    }
    write_units(memory, dest, &[], wide, true);
    ERANGE
}

fn units_at(memory: &mut dyn GuestMemory, va: u64, wide: bool) -> Vec<u16> {
    if wide { wstr(memory, va) } else { cstr(memory, va).into_iter().map(u16::from).collect() }
}

fn counted_units(memory: &mut dyn GuestMemory, va: u64, count: u64, wide: bool) -> Vec<u16> {
    let mut units = units_at(memory, va, wide);
    units.truncate(count.min(usize::MAX as u64) as usize);
    units
}

fn compare_units(a: &[u16], b: &[u16], fold: bool) -> i32 {
    let map = |u: u16| if fold { lower_unit(u) } else { u };
    for (x, y) in a.iter().map(|&u| map(u)).chain(std::iter::once(0)).zip(b.iter().map(|&u| map(u)).chain(std::iter::once(0))) {
        if x != y {
            return if x < y { -1 } else { 1 };
        }
        if x == 0 {
            break;
        }
    }
    0
}

fn to_radix(mut value: u64, radix: u32, negative: bool) -> Vec<u16> {
    let radix = radix.clamp(2, 36) as u64;
    let mut digits = Vec::new();
    loop {
        let d = (value % radix) as u32;
        digits.push(std::char::from_digit(d, radix as u32).unwrap_or('0') as u16);
        value /= radix;
        if value == 0 {
            break;
        }
    }
    if negative {
        digits.push(b'-' as u16);
    }
    digits.reverse();
    digits
}

/// The floating-point class codes of `_dclass`: FP_INFINITE 1, FP_NAN 2,
/// FP_NORMAL -1, FP_SUBNORMAL -2, FP_ZERO 0.
fn classify(value: f64) -> i32 {
    match value.classify() {
        std::num::FpCategory::Infinite => 1,
        std::num::FpCategory::Nan => 2,
        std::num::FpCategory::Normal => -1,
        std::num::FpCategory::Subnormal => -2,
        std::num::FpCategory::Zero => 0,
    }
}

fn erf(x: f64) -> f64 {
    // Abramowitz and Stegun 7.1.26.
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    if x < 0.0 { -y } else { y }
}

fn lgamma(x: f64) -> f64 {
    // Lanczos approximation.
    if x < 0.5 {
        return (std::f64::consts::PI / (std::f64::consts::PI * x).sin().abs()).ln() - lgamma(1.0 - x);
    }
    const G: [f64; 9] = [
        0.999_999_999_999_809_9, 676.5203681218851, -1259.1392167224028, 771.323_428_777_653_1, -176.615_029_162_140_6,
        12.507343278686905, -0.13857109526572012, 9.984_369_578_019_572e-6, 1.5056327351493116e-7,
    ];
    let x = x - 1.0;
    let mut a = G[0];
    let t = x + 7.5;
    for (i, g) in G.iter().enumerate().skip(1) {
        a += g / (x + i as f64);
    }
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

fn math(memory: &mut dyn GuestMemory, name: &str) -> Option<Option<u64>> {
    // Some(Some(int)) for integer results; Some(None) when XMM0 was set.
    let double = |memory: &mut dyn GuestMemory, f: fn(f64) -> f64| {
        let v = f(f64_arg(memory, 0));
        memory.set_xmm0(v.to_bits());
        Some(None)
    };
    let single = |memory: &mut dyn GuestMemory, f: fn(f32) -> f32| {
        let v = f(f32_arg(memory, 0));
        memory.set_xmm0(v.to_bits() as u64);
        Some(None)
    };
    let double2 = |memory: &mut dyn GuestMemory, f: fn(f64, f64) -> f64| {
        let v = f(f64_arg(memory, 0), f64_arg(memory, 1));
        memory.set_xmm0(v.to_bits());
        Some(None)
    };
    let single2 = |memory: &mut dyn GuestMemory, f: fn(f32, f32) -> f32| {
        let v = f(f32_arg(memory, 0), f32_arg(memory, 1));
        memory.set_xmm0(v.to_bits() as u64);
        Some(None)
    };
    let int = |v: i64| Some(Some(v as u64));
    match name {
        "fabs" => double(memory, f64::abs),
        "sqrt" => double(memory, f64::sqrt),
        "sqrtf" => single(memory, f32::sqrt),
        "sin" => double(memory, f64::sin),
        "sinf" => single(memory, f32::sin),
        "cos" => double(memory, f64::cos),
        "cosf" => single(memory, f32::cos),
        "tan" => double(memory, f64::tan),
        "tanf" => single(memory, f32::tan),
        "asin" => double(memory, f64::asin),
        "asinf" => single(memory, f32::asin),
        "acos" => double(memory, f64::acos),
        "acosf" => single(memory, f32::acos),
        "atan" => double(memory, f64::atan),
        "atanf" => single(memory, f32::atan),
        "sinh" => double(memory, f64::sinh),
        "sinhf" => single(memory, f32::sinh),
        "cosh" => double(memory, f64::cosh),
        "coshf" => single(memory, f32::cosh),
        "tanh" => double(memory, f64::tanh),
        "tanhf" => single(memory, f32::tanh),
        "asinh" => double(memory, f64::asinh),
        "asinhf" => single(memory, f32::asinh),
        "acosh" => double(memory, f64::acosh),
        "acoshf" => single(memory, f32::acosh),
        "atanh" => double(memory, f64::atanh),
        "atanhf" => single(memory, f32::atanh),
        "exp" => double(memory, f64::exp),
        "expf" => single(memory, f32::exp),
        "exp2" => double(memory, f64::exp2),
        "exp2f" => single(memory, f32::exp2),
        "expm1" => double(memory, f64::exp_m1),
        "expm1f" => single(memory, f32::exp_m1),
        "log" => double(memory, f64::ln),
        "logf" => single(memory, f32::ln),
        "log10" => double(memory, f64::log10),
        "log10f" => single(memory, f32::log10),
        "log2" => double(memory, f64::log2),
        "log2f" => single(memory, f32::log2),
        "log1p" => double(memory, f64::ln_1p),
        "log1pf" => single(memory, f32::ln_1p),
        "cbrt" => double(memory, f64::cbrt),
        "cbrtf" => single(memory, f32::cbrt),
        "ceil" => double(memory, f64::ceil),
        "ceilf" => single(memory, f32::ceil),
        "floor" => double(memory, f64::floor),
        "floorf" => single(memory, f32::floor),
        "trunc" => double(memory, f64::trunc),
        "truncf" => single(memory, f32::trunc),
        "round" => double(memory, f64::round),
        "roundf" => single(memory, f32::round),
        "rint" | "nearbyint" => double(memory, f64::round_ties_even),
        "rintf" | "nearbyintf" => single(memory, f32::round_ties_even),
        "erf" => double(memory, erf),
        "erff" => single(memory, |x| erf(x as f64) as f32),
        "lgamma" | "lgammal" => double(memory, lgamma),
        "lgammaf" => single(memory, |x| lgamma(x as f64) as f32),
        "atan2" => double2(memory, f64::atan2),
        "atan2f" => single2(memory, f32::atan2),
        "pow" => double2(memory, f64::powf),
        "powf" => single2(memory, f32::powf),
        "fmod" => double2(memory, |a, b| a % b),
        "fmodf" => single2(memory, |a, b| a % b),
        "hypot" | "_hypot" => double2(memory, f64::hypot),
        "_hypotf" => single2(memory, f32::hypot),
        "copysign" => double2(memory, f64::copysign),
        "copysignf" => single2(memory, f32::copysign),
        "fmax" => double2(memory, f64::max),
        "fmaxf" => single2(memory, f32::max),
        "fmin" => double2(memory, f64::min),
        "fminf" => single2(memory, f32::min),
        "remainder" => double2(memory, |a, b| a - (a / b).round_ties_even() * b),
        "remainderf" => single2(memory, |a, b| a - (a / b).round_ties_even() * b),
        "nextafter" | "nexttoward" => double2(memory, |a, b| {
            if a.is_nan() || b.is_nan() {
                return f64::NAN;
            }
            if a == b {
                return b;
            }
            if a == 0.0 {
                return f64::from_bits(1).copysign(b);
            }
            let bits = a.to_bits();
            f64::from_bits(if (b > a) == (a > 0.0) { bits + 1 } else { bits - 1 })
        }),
        "nextafterf" => single2(memory, |a, b| {
            if a.is_nan() || b.is_nan() {
                return f32::NAN;
            }
            if a == b {
                return b;
            }
            if a == 0.0 {
                return f32::from_bits(1).copysign(b);
            }
            let bits = a.to_bits();
            f32::from_bits(if (b > a) == (a > 0.0) { bits + 1 } else { bits - 1 })
        }),
        "fma" => {
            let v = f64_arg(memory, 0).mul_add(f64_arg(memory, 1), f64_arg(memory, 2));
            memory.set_xmm0(v.to_bits());
            Some(None)
        }
        "fmaf" => {
            let v = f32_arg(memory, 0).mul_add(f32_arg(memory, 1), f32_arg(memory, 2));
            memory.set_xmm0(v.to_bits() as u64);
            Some(None)
        }
        "lrint" | "llrint" => int(f64_arg(memory, 0).round_ties_even() as i64),
        "lrintf" | "llrintf" => int(f32_arg(memory, 0).round_ties_even() as i64),
        "lround" | "llround" | "llroundl" => int(f64_arg(memory, 0).round() as i64),
        "lroundf" | "llroundf" => int(f32_arg(memory, 0).round() as i64),
        "ilogb" => {
            let v = f64_arg(memory, 0);
            int(if v == 0.0 { i32::MIN as i64 } else if v.is_finite() { v.abs().log2().floor() as i64 } else { i32::MAX as i64 })
        }
        "ilogbf" => {
            let v = f32_arg(memory, 0) as f64;
            int(if v == 0.0 { i32::MIN as i64 } else if v.is_finite() { v.abs().log2().floor() as i64 } else { i32::MAX as i64 })
        }
        "_dclass" | "_ldclass" => int(classify(f64_arg(memory, 0)) as i64),
        "_fdclass" => int(classify(f32_arg(memory, 0) as f64) as i64),
        "_dsign" | "_ldsign" => int(i64::from(f64_arg(memory, 0).is_sign_negative()) * 0x8000),
        "_fdsign" => int(i64::from(f32_arg(memory, 0).is_sign_negative()) * 0x8000),
        "_isnan" => int(i64::from(f64_arg(memory, 0).is_nan())),
        "_finite" => int(i64::from(f64_arg(memory, 0).is_finite())),
        "_fpclass" => {
            let v = f64_arg(memory, 0);
            let negative = v.is_sign_negative();
            int(match v.classify() {
                std::num::FpCategory::Nan => 2,
                std::num::FpCategory::Infinite => if negative { 4 } else { 0x200 },
                std::num::FpCategory::Normal => if negative { 8 } else { 0x100 },
                std::num::FpCategory::Subnormal => if negative { 0x10 } else { 0x80 },
                std::num::FpCategory::Zero => if negative { 0x20 } else { 0x40 },
            })
        }
        _ => None,
    }
}

/// Perform a C library call.
pub fn call(ctx: &ApiContext, memory: &mut dyn GuestMemory, name: &str, a: &[u64]) -> Option<ApiOutcome> {
    let name = name.strip_prefix("_o_").unwrap_or(name);
    let count = arg_count(name)?;
    let arg = |i: usize| a.get(i).copied().unwrap_or(0);
    let p = |i: usize| arg(i) as u32;
    let ok = |value: u64| Some(ApiOutcome::wide(value, count));
    let signed = |value: i64| Some(ApiOutcome::wide(value as u64, count));
    let size = memory.ptr_size();
    if let Some(result) = math(memory, name) {
        return ok(result.unwrap_or(0));
    }
    match name {
        // ── Start-up and exit ──
        "_set_app_type" | "_set_fmode" | "__setusermatherr" | "_register_thread_local_exe_atexit_callback"
        | "_invalid_parameter_noinfo" | "_lock_locales" | "_unlock_locales" | "_tzset" | "_lock_file"
        | "_unlock_file" | "srand" | "_c_exit" | "_set_abort_behavior" | "_resetstkoflw" => {
            if name == "srand" {
                state(ctx, |s| s.seed = p(0));
            }
            ok(0)
        }
        "_configure_narrow_argv" | "_configure_wide_argv" | "_initialize_narrow_environment"
        | "_initialize_wide_environment" | "_configthreadlocale" | "_set_new_mode" | "_callnewh" | "fegetround"
        | "fesetround" | "_seh_filter_dll" | "_seh_filter_exe" | "___lc_codepage_func" | "___lc_collate_cp_func"
        | "_set_new_handler" | "_set_invalid_parameter_handler" | "_get_invalid_parameter_handler" | "_clearfp" => ok(0),
        "___mb_cur_max_func" => ok(1),
        "__p___argc" => {
            let (_, argc) = argv(ctx, memory, false);
            let block = static_block(ctx, memory, "argc_slot", 4);
            let _ = memory.write_u32(block, argc);
            ok(block as u64)
        }
        "__p___argv" | "__p___wargv" => {
            let (table, _) = argv(ctx, memory, name == "__p___wargv");
            let slot = static_block(ctx, memory, name, size);
            let _ = memory.write_ptr(slot, table as u64);
            ok(slot as u64)
        }
        "_get_initial_narrow_environment" | "_get_initial_wide_environment" => {
            ok(environment(ctx, memory, name.contains("wide")) as u64)
        }
        "__p__environ" | "__p__wenviron" => {
            let table = environment(ctx, memory, name == "__p__wenviron");
            let slot = static_block(ctx, memory, name, size);
            let _ = memory.write_ptr(slot, table as u64);
            ok(slot as u64)
        }
        "__p__commode" => ok(static_block(ctx, memory, "commode", 4) as u64),
        "_get_wide_winmain_command_line" | "_get_narrow_winmain_command_line" => {
            // The command line after the program's name.
            let line = command_line(ctx, memory);
            let mut i = 0;
            let mut quoted = false;
            while i < line.len() && (quoted || (line[i] != b' ' as u16 && line[i] != b'\t' as u16)) {
                if line[i] == b'"' as u16 {
                    quoted = !quoted;
                }
                i += 1;
            }
            while i < line.len() && (line[i] == b' ' as u16 || line[i] == b'\t' as u16) {
                i += 1;
            }
            let rest = &line[i..];
            ok(if name.contains("wide") {
                static_wstr(ctx, memory, rest)
            } else {
                static_cstr(ctx, memory, &unicode::wide_to_ansi(rest))
            } as u64)
        }
        "_initterm" | "_initterm_e" => {
            let (mut at, end) = (p(0), p(1));
            while at < end {
                let function = memory.read_ptr(at).unwrap_or(0);
                if function != 0 {
                    let result = guest(memory, function, &[]) as u32;
                    if name == "_initterm_e" && result != 0 {
                        return ok(result as u64);
                    }
                }
                at += size;
            }
            ok(0)
        }
        "_initialize_onexit_table" => {
            let _ = memory.write_bytes(p(0), &vec![0u8; size as usize * 3]);
            ok(0)
        }
        "_register_onexit_function" => {
            register_in_table(ctx, memory, p(0), arg(1));
            ok(0)
        }
        "_execute_onexit_table" => {
            execute_table(memory, p(0));
            ok(0)
        }
        "_crt_atexit" => {
            state(ctx, |s| s.atexit.push(arg(0)));
            ok(0)
        }
        "_cexit" => {
            run_atexit(ctx, memory);
            ok(0)
        }
        "exit" | "_exit" => {
            if name == "exit" {
                run_atexit(ctx, memory);
            }
            Some(ApiOutcome { value: p(0), wide: None, arg_count: count, exit_code: Some(p(0)) })
        }
        "abort" | "terminate" | "_purecall" | "_invalid_parameter_noinfo_noreturn" | "_wassert" | "raise" => {
            if name == "_wassert" {
                let message = String::from_utf16_lossy(&wstr(memory, arg(0)));
                let file = String::from_utf16_lossy(&wstr(memory, arg(1)));
                eprintln!("[ucrt] assertion failed: {message} ({file}:{})", p(2));
            } else {
                eprintln!("[ucrt] {name}()");
            }
            Some(ApiOutcome { value: 3, wide: None, arg_count: count, exit_code: Some(3) })
        }
        "_errno" => ok(static_block(ctx, memory, "errno", 8) as u64),
        "__doserrno" => ok(static_block(ctx, memory, "doserrno", 8) as u64),
        "_set_errno" | "_set_doserrno" => {
            let block = static_block(ctx, memory, if name == "_set_errno" { "errno" } else { "doserrno" }, 8);
            let _ = memory.write_u32(block, p(0));
            ok(0)
        }
        // ── Heap ──
        "malloc" | "_malloc_base" => ok(alloc(ctx, arg(0)) as u64),
        "calloc" | "_calloc_base" => {
            let bytes = arg(0).saturating_mul(arg(1));
            let block = alloc(ctx, bytes);
            if block != 0 {
                let _ = memory.write_bytes(block, &vec![0u8; bytes as usize]);
            }
            ok(block as u64)
        }
        "free" | "_free_base" => {
            release(ctx, p(0));
            ok(0)
        }
        "realloc" | "_recalloc" => {
            let (old, bytes) = if name == "realloc" { (p(0), arg(1)) } else { (p(0), arg(1).saturating_mul(arg(2))) };
            if old == 0 {
                let block = alloc(ctx, bytes);
                if name == "_recalloc" && block != 0 {
                    let _ = memory.write_bytes(block, &vec![0u8; bytes as usize]);
                }
                return ok(block as u64);
            }
            if bytes == 0 && name == "realloc" {
                release(ctx, old);
                return ok(0);
            }
            let old_size = ctx.shared.borrow().heap.size_of(old).unwrap_or(0) as u64;
            if bytes <= old_size {
                return ok(old as u64);
            }
            let block = alloc(ctx, bytes);
            if block == 0 {
                return ok(0);
            }
            let contents = memory.read_bytes(old, old_size as usize).unwrap_or_default();
            let _ = memory.write_bytes(block, &contents);
            if name == "_recalloc" {
                let _ = memory.write_bytes(block + old_size as u32, &vec![0u8; (bytes - old_size) as usize]);
            }
            release(ctx, old);
            ok(block as u64)
        }
        "_aligned_malloc" | "_aligned_offset_malloc" | "_aligned_realloc" => {
            let (bytes, alignment, offset, old) = match name {
                "_aligned_malloc" => (arg(0), arg(1), 0, 0),
                "_aligned_offset_malloc" => (arg(0), arg(1), arg(2), 0),
                _ => (arg(1), arg(2), 0, p(0)),
            };
            let alignment = alignment.max(16).next_power_of_two();
            let base = alloc(ctx, bytes + alignment + offset);
            if base == 0 {
                return ok(0);
            }
            let aligned = ((base as u64 + offset + alignment - 1) & !(alignment - 1)) - offset;
            state(ctx, |s| s.aligned.insert(aligned as u32, base));
            if old != 0 {
                let old_size = state(ctx, |s| s.aligned.get(&old).copied())
                    .and_then(|b| ctx.shared.borrow().heap.size_of(b).map(|size| size - (old - b)))
                    .unwrap_or(0);
                let contents = memory.read_bytes(old, old_size.min(bytes as u32) as usize).unwrap_or_default();
                let _ = memory.write_bytes(aligned as u32, &contents);
                if let Some(b) = state(ctx, |s| s.aligned.remove(&old)) {
                    release(ctx, b);
                }
            }
            ok(aligned)
        }
        "_aligned_free" => {
            if let Some(base) = state(ctx, |s| s.aligned.remove(&p(0))) {
                release(ctx, base);
            }
            ok(0)
        }
        "_strdup" | "_wcsdup" => {
            if arg(0) == 0 {
                return ok(0);
            }
            let wide = name == "_wcsdup";
            let text = units_at(memory, arg(0), wide);
            let block = alloc(ctx, (text.len() as u64 + 1) * if wide { 2 } else { 1 });
            write_units(memory, block, &text, wide, true);
            ok(block as u64)
        }
        // ── Strings ──
        "strlen" | "wcslen" => ok(units_at(memory, arg(0), name == "wcslen").len() as u64),
        "strnlen" | "wcsnlen" | "__strncnt" => {
            let wide = name == "wcsnlen";
            // Read page by page: a large limit must not run past the end of
            // the string's mapping (a string at the top of the stack).
            let limit = arg(1).min(1 << 30) as usize;
            let unit = if wide { 2 } else { 1 };
            let mut length = 0usize;
            while length < limit {
                let va = arg(0) + (length * unit) as u64;
                let chunk = ((0x1000 - (va & 0xFFF)) as usize / unit).clamp(1, limit - length);
                let units = read_units(memory, va, chunk, wide);
                if units.len() < chunk {
                    break;
                }
                if let Some(end) = units.iter().position(|&u| u == 0) {
                    length += end;
                    break;
                }
                length += chunk;
            }
            ok(length as u64)
        }
        "strcmp" | "wcscmp" | "_stricmp" | "_wcsicmp" | "_wcsicmp_l" => {
            let wide = name.starts_with('w') || name.starts_with("_w");
            let (x, y) = (units_at(memory, arg(0), wide), units_at(memory, arg(1), wide));
            signed(compare_units(&x, &y, name.contains("icmp")) as i64)
        }
        "strncmp" | "wcsncmp" | "_strnicmp" | "_wcsnicmp" => {
            let wide = name.starts_with('w') || name.starts_with("_w");
            let (x, y) = (counted_units(memory, arg(0), arg(2), wide), counted_units(memory, arg(1), arg(2), wide));
            signed(compare_units(&x, &y, name.contains("icmp")) as i64)
        }
        "strcpy" | "wcscpy" => {
            let wide = name == "wcscpy";
            let text = units_at(memory, arg(1), wide);
            write_units(memory, p(0), &text, wide, true);
            ok(arg(0))
        }
        "strncpy" | "wcsncpy" => {
            let wide = name == "wcsncpy";
            let mut text = counted_units(memory, arg(1), arg(2), wide);
            text.resize(arg(2) as usize, 0);
            write_units(memory, p(0), &text, wide, false);
            ok(arg(0))
        }
        "strcat" => {
            let mut text = units_at(memory, arg(0), false);
            text.extend(units_at(memory, arg(1), false));
            write_units(memory, p(0), &text, false, true);
            ok(arg(0))
        }
        "strncat" => {
            let mut text = units_at(memory, arg(0), false);
            text.extend(counted_units(memory, arg(1), arg(2), false));
            write_units(memory, p(0), &text, false, true);
            ok(arg(0))
        }
        "strcpy_s" | "wcscpy_s" => {
            let wide = name == "wcscpy_s";
            let text = units_at(memory, arg(2), wide);
            ok(secure_copy(memory, p(0), arg(1), &text, wide, false) as u64)
        }
        "strncpy_s" | "wcsncpy_s" => {
            let wide = name == "wcsncpy_s";
            let text = counted_units(memory, arg(2), arg(3), wide);
            ok(secure_copy(memory, p(0), arg(1), &text, wide, arg(3) == TRUNCATE) as u64)
        }
        "strcat_s" | "wcscat_s" => {
            let wide = name == "wcscat_s";
            let mut text = units_at(memory, arg(0), wide);
            text.extend(units_at(memory, arg(2), wide));
            ok(secure_copy(memory, p(0), arg(1), &text, wide, false) as u64)
        }
        "memcpy_s" | "wmemcpy_s" => {
            let unit = if name == "wmemcpy_s" { 2 } else { 1 };
            let (capacity, length) = (arg(1) * unit, arg(3) * unit);
            if length == 0 {
                return ok(0);
            }
            if p(0) == 0 {
                return ok(EINVAL as u64);
            }
            if length > capacity || p(2) == 0 {
                let _ = memory.write_bytes(p(0), &vec![0u8; capacity as usize]);
                return ok(if p(2) == 0 { EINVAL } else { ERANGE } as u64);
            }
            let bytes = memory.read_bytes(p(2), length as usize).unwrap_or_default();
            let _ = memory.write_bytes(p(0), &bytes);
            ok(0)
        }
        "strcspn" | "strspn" | "strpbrk" | "wcspbrk" => {
            let wide = name == "wcspbrk";
            let (text, set) = (units_at(memory, arg(0), wide), units_at(memory, arg(1), wide));
            let position = match name {
                "strspn" => text.iter().position(|u| !set.contains(u)).unwrap_or(text.len()),
                _ => text.iter().position(|u| set.contains(u)).unwrap_or(text.len()),
            };
            match name {
                "strcspn" | "strspn" => ok(position as u64),
                _ => ok(if position < text.len() { arg(0) + position as u64 * if wide { 2 } else { 1 } } else { 0 }),
            }
        }
        "wcstok_s" => {
            // wcstok_s(string, delimiters, &context)
            let start = if arg(0) != 0 { arg(0) } else { memory.read_ptr(p(2)).unwrap_or(0) };
            if start == 0 {
                return ok(0);
            }
            let set = wstr(memory, arg(1));
            let text = wstr(memory, start);
            let Some(first) = text.iter().position(|u| !set.contains(u)) else {
                let _ = memory.write_ptr(p(2), start + text.len() as u64 * 2);
                return ok(0);
            };
            let end = text[first..].iter().position(|u| set.contains(u)).map(|e| e + first);
            let token = start + first as u64 * 2;
            match end {
                Some(e) => {
                    let _ = memory.write_bytes((start + e as u64 * 2) as u32, &[0, 0]);
                    let _ = memory.write_ptr(p(2), start + (e as u64 + 1) * 2);
                }
                None => {
                    let _ = memory.write_ptr(p(2), start + text.len() as u64 * 2);
                }
            }
            ok(token)
        }
        "_wcslwr" | "_wcsupr_s" => {
            let text: Vec<u16> = wstr(memory, arg(0))
                .into_iter()
                .map(|u| if name == "_wcslwr" { lower_unit(u) } else { upper_unit(u) })
                .collect();
            put_w(memory, p(0), &text);
            ok(if name == "_wcslwr" { arg(0) } else { 0 })
        }
        "strerror" => ok(static_cstr(ctx, memory, error_text(p(0)).as_bytes()) as u64),
        "strerror_s" => {
            let text: Vec<u16> = ascii(error_text(p(2)));
            ok(secure_copy(memory, p(0), arg(1), &text, false, true).min(1) as u64 * 0)
        }
        "_mbsinc" => ok(arg(0) + 1),
        "_mbsdec" => ok(if arg(1) > arg(0) { arg(1) - 1 } else { 0 }),
        "wcstombs" => {
            let text = unicode::wide_to_ansi(&wstr(memory, arg(1)));
            if p(0) == 0 {
                return ok(text.len() as u64);
            }
            let n = text.len().min(arg(2) as usize);
            let mut bytes = text[..n].to_vec();
            if n < arg(2) as usize {
                bytes.push(0);
            }
            let _ = memory.write_bytes(p(0), &bytes);
            ok(n as u64)
        }
        "mbsrtowcs" => {
            // mbsrtowcs(dest, &source, count, state)
            let source = memory.read_ptr(p(1)).unwrap_or(0);
            let text = unicode::ansi_to_wide(&cstr(memory, source));
            if p(0) == 0 {
                return ok(text.len() as u64);
            }
            let n = text.len().min(arg(2) as usize);
            let complete = n == text.len() && n < arg(2) as usize;
            write_units(memory, p(0), &text[..n], true, complete);
            let _ = memory.write_ptr(p(1), if complete { 0 } else { source + n as u64 });
            ok(n as u64)
        }
        // ── Characters ──
        "tolower" | "toupper" => {
            let c = p(0) as i32;
            if !(0..256).contains(&c) {
                return signed(c as i64);
            }
            let b = c as u8;
            ok(if name == "tolower" { b.to_ascii_lowercase() } else { b.to_ascii_uppercase() } as u64)
        }
        "towlower" | "_towlower_l" => ok(lower_unit(p(0) as u16) as u64),
        "towupper" | "_towupper_l" => ok(upper_unit(p(0) as u16) as u64),
        "isalnum" | "isalpha" | "isdigit" | "islower" | "isupper" | "isprint" | "isspace" | "isxdigit" => {
            let flags = ctype(p(0));
            let set = match name {
                "isalnum" => ALPHA | DIGIT,
                "isalpha" => ALPHA,
                "isdigit" => DIGIT,
                "islower" => LOWER,
                "isupper" => UPPER,
                "isspace" => SPACE,
                "isxdigit" => HEX,
                _ => {
                    return ok(u64::from((0x20..0x7F).contains(&p(0))));
                }
            };
            ok((flags & set) as u64)
        }
        "iswdigit" => ok(u64::from((b'0' as u32..=b'9' as u32).contains(&(p(0) & 0xFFFF)))),
        "iswspace" => ok(u64::from(char::from_u32(p(0) & 0xFFFF).is_some_and(char::is_whitespace))),
        "iswalpha" => ok(u64::from(char::from_u32(p(0) & 0xFFFF).is_some_and(char::is_alphabetic))),
        "__pctype_func" => {
            let key = "pctype";
            if let Some(va) = state(ctx, |s| s.statics.get(key).copied()) {
                return ok(va as u64 + 2);
            }
            let table = alloc(ctx, 258 * 2);
            let mut bytes = vec![0u8; 2];
            for c in 0..256u32 {
                bytes.extend_from_slice(&ctype(c).to_le_bytes());
            }
            bytes.extend_from_slice(&[0, 0]);
            let _ = memory.write_bytes(table, &bytes);
            state(ctx, |s| s.statics.insert(key.into(), table));
            ok(table as u64 + 2)
        }
        // ── Numbers ──
        "strtol" | "strtoul" | "strtoll" | "strtoull" | "_strtoui64" | "wcstol" | "wcstoul" | "wcstoull" | "_wcstoui64"
        | "atoi" | "atol" | "_atoi64" | "_wtoi" => {
            let wide = name.starts_with('w') || name.starts_with("_w");
            let text = units_at(memory, arg(0), wide);
            let base = if name.contains("ato") || name == "_wtoi" { 10 } else { p(2) };
            let (value, negative, used, overflow) = parse_integer(&text, base);
            if !name.contains("ato") && name != "_wtoi" && arg(1) != 0 {
                let _ = memory.write_ptr(p(1), if used == 0 { arg(0) } else { arg(0) + used as u64 * if wide { 2 } else { 1 } });
            }
            let bits = match name {
                "strtoll" | "strtoull" | "_strtoui64" | "wcstoull" | "_wcstoui64" | "_atoi64" => 64,
                _ => 32,
            };
            let unsigned = name.contains("toul") || name.contains("toui");
            let result: u64 = if unsigned {
                let max = if bits == 64 { u64::MAX } else { u32::MAX as u64 };
                if overflow || value > max {
                    set_errno(ctx, memory, ERANGE);
                    max
                } else if negative {
                    value.wrapping_neg() & max
                } else {
                    value
                }
            } else {
                let (min, max) = if bits == 64 { (i64::MIN as i128, i64::MAX as i128) } else { (i32::MIN as i128, i32::MAX as i128) };
                let v = if negative { -(value as i128) } else { value as i128 };
                let clamped = if overflow { if negative { min } else { max } } else { v.clamp(min, max) };
                if clamped != v || overflow {
                    set_errno(ctx, memory, ERANGE);
                }
                clamped as i64 as u64
            };
            ok(if bits == 32 && !unsigned { result as i32 as i64 as u64 } else { result })
        }
        "strtod" | "strtof" | "wcstod" | "wcstof" | "atof" => {
            let wide = name.starts_with('w');
            let text = units_at(memory, arg(0), wide);
            let (value, used) = parse_float(&text);
            if name != "atof" && arg(1) != 0 {
                let _ = memory.write_ptr(p(1), arg(0) + used as u64 * if wide { 2 } else { 1 });
            }
            if name.ends_with('f') && name != "atof" {
                memory.set_xmm0((value as f32).to_bits() as u64);
            } else {
                memory.set_xmm0(value.to_bits());
            }
            ok(0)
        }
        "_i64toa" | "_ui64toa" => {
            let negative = name == "_i64toa" && (arg(0) as i64) < 0 && p(2) == 10;
            let magnitude = if negative { (arg(0) as i64).unsigned_abs() } else { arg(0) };
            write_units(memory, p(1), &to_radix(magnitude, p(2), negative), false, true);
            ok(arg(1))
        }
        "_ltoa_s" | "_i64tow_s" => {
            let wide = name == "_i64tow_s";
            let value = if wide { arg(0) as i64 } else { p(0) as i32 as i64 };
            let negative = value < 0 && p(3) == 10;
            let magnitude = if negative { value.unsigned_abs() } else if wide { value as u64 } else { value as u32 as u64 };
            let text = to_radix(magnitude, p(3), negative);
            ok(secure_copy(memory, p(1), arg(2), &text, wide, false) as u64)
        }
        "abs" => signed((p(0) as i32).unsigned_abs() as i64),
        "llabs" => signed((arg(0) as i64).unsigned_abs() as i64),
        "div" => {
            let (n, d) = (p(0) as i32, p(1) as i32);
            if d == 0 {
                return ok(0);
            }
            ok(((n / d) as u32 as u64) | (((n % d) as u32 as u64) << 32))
        }
        "lldiv" | "imaxdiv" => {
            // A 16-byte structure: returned through the hidden first argument.
            let (n, d) = (arg(1) as i64, arg(2) as i64);
            if d != 0 {
                let _ = memory.write_u64(p(0), (n / d) as u64);
                let _ = memory.write_u64(p(0) + 8, (n % d) as u64);
            }
            ok(arg(0))
        }
        "_rotl" => ok((p(0).rotate_left(p(1) & 31)) as u64),
        "rand" => {
            let next = state(ctx, |s| {
                s.seed = s.seed.wrapping_mul(214013).wrapping_add(2531011);
                (s.seed >> 16) & 0x7FFF
            });
            ok(next as u64)
        }
        "rand_s" => {
            let mut value = [0u8; 4];
            let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos();
            let mixed = seed.wrapping_mul(0x9E37_79B9) ^ state(ctx, |s| {
                s.seed = s.seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                s.seed
            });
            value.copy_from_slice(&mixed.to_le_bytes());
            let _ = memory.write_bytes(p(0), &value);
            ok(0)
        }
        "qsort" => {
            sort(ctx, memory, p(0), arg(1) as usize, arg(2) as usize, arg(3));
            ok(0)
        }
        "bsearch" => {
            // bsearch(key, base, count, width, compare)
            let (mut low, mut high) = (0u64, arg(2));
            while low < high {
                let middle = (low + high) / 2;
                let element = arg(1) + middle * arg(3);
                let order = guest(memory, arg(4), &[arg(0), element]) as u32 as i32;
                match order {
                    0 => return ok(element),
                    o if o < 0 => high = middle,
                    _ => low = middle + 1,
                }
            }
            ok(0)
        }
        "nan" | "nanf" => {
            if name == "nan" {
                memory.set_xmm0(f64::NAN.to_bits());
            } else {
                memory.set_xmm0(f32::NAN.to_bits() as u64);
            }
            ok(0)
        }
        "ldexp" | "scalbn" => {
            let v = f64_arg(memory, 0) * 2f64.powi(p(1) as i32);
            memory.set_xmm0(v.to_bits());
            ok(0)
        }
        "scalbnf" => {
            let v = f32_arg(memory, 0) * 2f32.powi(p(1) as i32);
            memory.set_xmm0(v.to_bits() as u64);
            ok(0)
        }
        "frexp" => {
            let v = f64_arg(memory, 0);
            let (mantissa, exponent) = if v == 0.0 || !v.is_finite() {
                (v, 0)
            } else {
                let e = v.abs().log2().floor() as i32 + 1;
                let m = v / 2f64.powi(e);
                if m.abs() >= 1.0 { (m / 2.0, e + 1) } else if m.abs() < 0.5 { (m * 2.0, e - 1) } else { (m, e) }
            };
            let _ = memory.write_u32(p(1), exponent as u32);
            memory.set_xmm0(mantissa.to_bits());
            ok(0)
        }
        "modf" => {
            let v = f64_arg(memory, 0);
            let whole = v.trunc();
            let _ = memory.write_u64(p(1), whole.to_bits());
            memory.set_xmm0(if v.is_infinite() { 0.0f64.copysign(v) } else { v - whole }.to_bits());
            ok(0)
        }
        "modff" => {
            let v = f32_arg(memory, 0);
            let whole = v.trunc();
            let _ = memory.write_u32(p(1), whole.to_bits());
            memory.set_xmm0((v - whole).to_bits() as u64);
            ok(0)
        }
        "_dtest" | "_ldtest" => {
            let v = f64::from_bits(memory.read_u64(p(0)).unwrap_or(0));
            signed(classify(v) as i64)
        }
        "_fdtest" => {
            let v = f32::from_bits(memory.read_u32(p(0)).unwrap_or(0));
            signed(classify(v as f64) as i64)
        }
        "_control87" => ok(0x0009_001F),
        "_controlfp_s" => {
            if p(0) != 0 {
                let _ = memory.write_u32(p(0), 0x0009_001F);
            }
            ok(0)
        }
        // ── Locale: the "C" locale ──
        "setlocale" => ok(static_cstr(ctx, memory, b"C") as u64),
        "_wsetlocale" => ok(static_wstr(ctx, memory, &ascii("C")) as u64),
        "_create_locale" => {
            let block = alloc(ctx, 64);
            let _ = memory.write_bytes(block, &[0u8; 64]);
            ok(block as u64)
        }
        "_free_locale" => {
            release(ctx, p(0));
            ok(0)
        }
        "___lc_locale_name_func" => ok(static_block(ctx, memory, "lc_names", size * 6) as u64),
        "localeconv" => {
            if let Some(va) = state(ctx, |s| s.statics.get("lconv").copied()) {
                return ok(va as u64);
            }
            // lconv: 10 char*, 8 chars, 4 more chars, 6 wchar_t*.
            let block = alloc(ctx, size as u64 * 18 + 16);
            let _ = memory.write_bytes(block, &vec![0u8; size as usize * 18 + 16]);
            let empty = static_cstr(ctx, memory, b"");
            let dot = static_cstr(ctx, memory, b".");
            for index in 0..10 {
                let _ = memory.write_ptr(block + index * size, if index == 0 { dot } else { empty } as u64);
            }
            let _ = memory.write_bytes(block + 10 * size, &[127u8; 8]);
            let wide_at = block + 10 * size + 8 + (size - 8 % size) % size;
            let wempty = static_wstr(ctx, memory, &[]);
            let wdot = static_wstr(ctx, memory, &ascii("."));
            for index in 0..6 {
                let _ = memory.write_ptr(wide_at + index * size, if index == 0 { wdot } else { wempty } as u64);
            }
            state(ctx, |s| s.statics.insert("lconv".into(), block));
            ok(block as u64)
        }
        "_Getdays" | "_Getmonths" => {
            let text = if name == "_Getdays" { time_names() } else { month_names() };
            let block = alloc(ctx, text.len() as u64 + 1);
            put_c(memory, block, text.as_bytes());
            ok(block as u64)
        }
        "_W_Getdays" | "_W_Getmonths" => {
            let text = if name == "_W_Getdays" { time_names() } else { month_names() };
            let units: Vec<u16> = text.encode_utf16().collect();
            let block = alloc(ctx, units.len() as u64 * 2 + 2);
            put_w(memory, block, &units);
            ok(block as u64)
        }
        "_Gettnames" | "_W_Gettnames" => {
            let block = alloc(ctx, 0x400);
            let _ = memory.write_bytes(block, &[0u8; 0x400]);
            ok(block as u64)
        }
        // ── Environment ──
        "getenv" | "_wgetenv" => {
            let wide = name == "_wgetenv";
            let key = path_arg(memory, arg(0), wide);
            let buffer = alloc(ctx, 4096);
            let mut name_bytes = key.clone();
            name_bytes.push(0);
            let name_va = alloc(ctx, name_bytes.len() as u64);
            let _ = memory.write_bytes(name_va, &name_bytes);
            let length = win32(ctx, memory, "KERNEL32.dll", "GetEnvironmentVariableA", &[name_va, buffer, 4096]);
            let value = if length > 0 && length < 4096 { Some(read_cstr(memory, buffer)) } else { None };
            release(ctx, buffer);
            release(ctx, name_va);
            ok(match value {
                Some(value) if wide => static_wstr(ctx, memory, &unicode::ansi_to_wide(&value)) as u64,
                Some(value) => static_cstr(ctx, memory, &value) as u64,
                None => 0,
            })
        }
        "getenv_s" => {
            // getenv_s(&required, buffer, size, name): not set.
            let _ = memory.write_ptr(p(0), 0);
            if p(1) != 0 && arg(2) > 0 {
                let _ = memory.write_bytes(p(1), &[0]);
            }
            ok(0)
        }
        "_putenv" | "_wputenv" => {
            // "NAME=value"; an empty value removes the variable.
            let text = String::from_utf8_lossy(&path_arg(memory, arg(0), name == "_wputenv")).into_owned();
            let Some((key, value)) = text.split_once('=') else {
                return ok(u32::MAX as u64);
            };
            crate::win32::env::set(ctx, key, (!value.is_empty()).then_some(value));
            ok(0)
        }
        "_putenv_s" | "_wputenv_s" => {
            let wide = name == "_wputenv_s";
            let key = String::from_utf8_lossy(&path_arg(memory, arg(0), wide)).into_owned();
            let value = String::from_utf8_lossy(&path_arg(memory, arg(1), wide)).into_owned();
            crate::win32::env::set(ctx, &key, (!value.is_empty()).then_some(value.as_str()));
            ok(0)
        }
        // ── Time ──
        "_time64" => {
            let (secs, _) = now();
            if p(0) != 0 {
                let _ = memory.write_u64(p(0), secs as u64);
            }
            ok(secs as u64)
        }
        "clock" => {
            let started = state(ctx, |s| *s.started.get_or_insert_with(std::time::Instant::now));
            ok(started.elapsed().as_millis() as u64)
        }
        "_ftime64" => {
            let (secs, millis) = now();
            let mut bytes = secs.to_le_bytes().to_vec();
            bytes.extend_from_slice(&(millis as u16).to_le_bytes());
            bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            let _ = memory.write_bytes(p(0), &bytes);
            ok(0)
        }
        "_gmtime64" | "_localtime64" => {
            let secs = memory.read_u64(p(0)).unwrap_or(0) as i64;
            let block = static_block(ctx, memory, "tm", 36);
            write_tm(memory, block, &broken_down(secs));
            ok(block as u64)
        }
        "_gmtime64_s" | "_localtime64_s" => {
            let secs = memory.read_u64(p(1)).unwrap_or(0) as i64;
            write_tm(memory, p(0), &broken_down(secs));
            ok(0)
        }
        "_mktime64" => {
            let tm = read_tm(memory, p(0));
            let secs = time_of(&tm);
            write_tm(memory, p(0), &broken_down(secs));
            ok(secs as u64)
        }
        "_difftime64" => {
            let v = (arg(0) as i64 - arg(1) as i64) as f64;
            memory.set_xmm0(v.to_bits());
            ok(0)
        }
        "asctime" => {
            let tm = read_tm(memory, p(0));
            let text = format!(
                "{} {} {:2} {:02}:{:02}:{:02} {}\n",
                &DAYS[tm[6].rem_euclid(7) as usize][..3],
                &MONTHS[tm[4].rem_euclid(12) as usize][..3],
                tm[3],
                tm[2],
                tm[1],
                tm[0],
                tm[5] + 1900
            );
            let block = static_block(ctx, memory, "asctime", 32);
            put_c(memory, block, text.as_bytes());
            ok(block as u64)
        }
        "strftime" | "_Strftime" | "wcsftime" | "_Wcsftime" => {
            let wide = name.contains("cs");
            let fmt = units_at(memory, arg(2), wide);
            let tm = read_tm(memory, p(3));
            let text = output_for(&strftime(&fmt, &tm), wide);
            if (text.len() as u64) < arg(1) {
                write_units(memory, p(0), &text, wide, true);
                ok(text.len() as u64)
            } else {
                ok(0)
            }
        }
        "__timezone" | "__daylight" => ok(static_block(ctx, memory, name, 8) as u64),
        "__tzname" => {
            if let Some(va) = state(ctx, |s| s.statics.get("tzname").copied()) {
                return ok(va as u64);
            }
            let block = alloc(ctx, size as u64 * 2);
            let utc = static_cstr(ctx, memory, b"UTC");
            let _ = memory.write_ptr(block, utc as u64);
            let _ = memory.write_ptr(block + size, utc as u64);
            state(ctx, |s| s.statics.insert("tzname".into(), block));
            ok(block as u64)
        }
        // ── Threads and processes ──
        "_beginthreadex" => {
            // _beginthreadex(security, stack, start, argument, flags, &id)
            let handle = win32(ctx, memory, "KERNEL32.dll", "CreateThread", &[p(0), p(1), p(2), p(3), p(4), p(5)]);
            ok(handle as u64)
        }
        "_endthreadex" => {
            let outcome = crate::win32::dispatch_with_memory(
                ctx,
                memory,
                "KERNEL32.dll",
                &ImportName::ByName("ExitThread".into()),
                &[p(0)],
                0,
            );
            outcome.map(|o| ApiOutcome { arg_count: count, ..o })
        }
        "_getpid" => ok(1),
        "_getdrives" => ok(4 | if crt::drive_d_root().is_some() { 8 } else { 0 }),
        // ── Files and descriptors ──
        "__acrt_iob_func" => ok(standard_stream(ctx, memory, p(0).min(2)) as u64),
        "_open" | "_wopen" | "_sopen_s" | "_wsopen_s" | "_wsopen_dispatch" => {
            let (path, flags, out) = match name {
                "_open" | "_wopen" => (arg(0), p(1), 0),
                "_wsopen_dispatch" => (arg(0), p(1), p(4)),
                _ => (arg(1), p(2), p(0)),
            };
            let path = path_arg(memory, path, name.contains('w'));
            let result = open_path(ctx, &path, flags);
            match (result, out) {
                (Ok(fd), 0) => ok(fd as u64),
                (Ok(fd), out) => {
                    let _ = memory.write_u32(out, fd as u32);
                    ok(0)
                }
                (Err(code), 0) => {
                    set_errno(ctx, memory, code);
                    signed(-1)
                }
                (Err(code), out) => {
                    let _ = memory.write_u32(out, u32::MAX);
                    ok(code as u64)
                }
            }
        }
        "_close" => {
            if close_fd(ctx, p(0) as i32) {
                ok(0)
            } else {
                set_errno(ctx, memory, EBADF);
                signed(-1)
            }
        }
        "_read" => match fd_read(ctx, p(0) as i32, p(2) as usize) {
            Some(bytes) => {
                let _ = memory.write_bytes(p(1), &bytes);
                ok(bytes.len() as u64)
            }
            None => signed(-1),
        },
        "_write" => {
            let bytes = memory.read_bytes(p(1), p(2) as usize).unwrap_or_default();
            match fd_write(ctx, p(0) as i32, &bytes) {
                Some(n) => ok(n as u64),
                None => signed(-1),
            }
        }
        "_lseek" => match fd_of(ctx, p(0) as i32) {
            Some(Fd::File(handle)) => match crt::seek_file(ctx, handle, p(1) as i32 as i64, p(2)) {
                Some(position) => ok(position),
                None => signed(-1),
            },
            _ => signed(-1),
        },
        "_chsize" => ok(0),
        "_isatty" => ok(u64::from((0..=2).contains(&(p(0) as i32)))),
        "_dup" => match fd_of(ctx, p(0) as i32) {
            Some(fd) => ok(new_fd(ctx, fd) as u64),
            None => signed(-1),
        },
        "_setmode" => ok(0x4000),
        "_get_osfhandle" => match fd_of(ctx, p(0) as i32) {
            Some(Fd::File(handle)) => ok(handle as u64),
            Some(Fd::Std(index)) => {
                let handle = win32(ctx, memory, "KERNEL32.dll", "GetStdHandle", &[(-10i32 - index as i32) as u32]);
                ok(crate::kernel::syscall::widen(handle))
            }
            None => signed(-1),
        },
        "_open_osfhandle" => ok(new_fd(ctx, Fd::File(p(0))) as u64),
        "_fstat64i32" => match fd_of(ctx, p(0) as i32) {
            Some(Fd::File(handle)) => {
                let metadata = ctx.shared.borrow().files.get(&handle).and_then(|f| f.metadata().ok());
                match metadata {
                    Some(metadata) => {
                        write_stat(memory, p(1), &metadata, true);
                        ok(0)
                    }
                    None => signed(-1),
                }
            }
            _ => {
                let _ = memory.write_bytes(p(1), &[0u8; 48]);
                let _ = memory.write_bytes(p(1) + 6, &0x2000u16.to_le_bytes());
                ok(0)
            }
        },
        "_wstat64" | "_wstat64i32" => {
            let path = path_arg(memory, arg(0), true);
            match crt::host_path_in(ctx, &path).and_then(|host| std::fs::metadata(host).ok()) {
                Some(metadata) => {
                    write_stat(memory, p(1), &metadata, name == "_wstat64i32");
                    ok(0)
                }
                None => {
                    set_errno(ctx, memory, ENOENT);
                    signed(-1)
                }
            }
        }
        "_access" | "_waccess" => {
            let path = path_arg(memory, arg(0), name == "_waccess");
            if crt::host_path_in(ctx, &path).is_some_and(|host| host.exists()) {
                ok(0)
            } else {
                set_errno(ctx, memory, ENOENT);
                signed(-1)
            }
        }
        "_mkdir" | "_wrmdir" | "remove" | "_wremove" => {
            let path = path_arg(memory, arg(0), name.starts_with("_w"));
            let result = crt::host_path_in(ctx, &path).map(|host| match name {
                "_mkdir" => std::fs::create_dir(host),
                "_wrmdir" => std::fs::remove_dir(host),
                _ => std::fs::remove_file(host),
            });
            match result {
                Some(Ok(())) => ok(0),
                _ => {
                    set_errno(ctx, memory, ENOENT);
                    signed(-1)
                }
            }
        }
        "_wrename" => {
            let (from, to) = (path_arg(memory, arg(0), true), path_arg(memory, arg(1), true));
            match (crt::host_path_in(ctx, &from), crt::host_path_in(ctx, &to)) {
                (Some(from), Some(to)) if std::fs::rename(&from, &to).is_ok() => ok(0),
                _ => {
                    set_errno(ctx, memory, ENOENT);
                    signed(-1)
                }
            }
        }
        "_wchmod" => ok(0),
        "_chdir" | "_wchdir" => {
            let function = if name == "_chdir" { "SetCurrentDirectoryA" } else { "SetCurrentDirectoryW" };
            let done = win32(ctx, memory, "KERNEL32.dll", function, &[p(0)]);
            if done != 0 { ok(0) } else { signed(-1) }
        }
        "_wgetcwd" | "_wgetdcwd" => {
            let (buffer, capacity) = if name == "_wgetcwd" { (p(0), p(1)) } else { (p(1), p(2)) };
            let scratch = alloc(ctx, 1024);
            let length = win32(ctx, memory, "KERNEL32.dll", "GetCurrentDirectoryW", &[512, scratch]);
            let text = read_units(memory, scratch as u64, length as usize, true);
            release(ctx, scratch);
            let target = if buffer == 0 { alloc(ctx, (text.len() as u64 + 1).max(capacity as u64) * 2) } else { buffer };
            if buffer != 0 && text.len() as u32 >= capacity {
                set_errno(ctx, memory, ERANGE);
                return ok(0);
            }
            put_w(memory, target, &text);
            ok(target as u64)
        }
        "_fullpath" | "_wfullpath" => {
            let wide = name == "_wfullpath";
            let relative = path_arg(memory, arg(1), wide);
            let absolute = crt::absolute(ctx, &relative);
            let text: Vec<u16> = if wide { unicode::ansi_to_wide(&absolute) } else { absolute.iter().map(|&b| b as u16).collect() };
            let target = if p(0) == 0 { alloc(ctx, (text.len() as u64 + 1) * 2) } else { p(0) };
            if p(0) != 0 && text.len() as u64 >= arg(2) {
                return ok(0);
            }
            write_units(memory, target, &text, wide, true);
            ok(target as u64)
        }
        "_splitpath_s" => {
            // (path, drive, size, dir, size, name, size, ext, size)
            let path = units_at(memory, arg(0), false);
            let drive_end = if path.get(1) == Some(&(b':' as u16)) { 2 } else { 0 };
            let dir_end = path.iter().rposition(|&u| u == b'\\' as u16 || u == b'/' as u16).map(|i| i + 1).unwrap_or(drive_end).max(drive_end);
            let ext_start = path[dir_end..].iter().rposition(|&u| u == b'.' as u16).map(|i| i + dir_end).unwrap_or(path.len());
            for (index, part) in [&path[..drive_end], &path[drive_end..dir_end], &path[dir_end..ext_start], &path[ext_start..]]
                .into_iter()
                .enumerate()
            {
                let (target, capacity) = (p(1 + index * 2), arg(2 + index * 2));
                if target != 0 {
                    secure_copy(memory, target, capacity, part, false, false);
                }
            }
            ok(0)
        }
        "_makepath_s" => {
            // (path, size, drive, dir, name, ext)
            let mut text = Vec::new();
            for index in 2..6 {
                let part = units_at(memory, arg(index), false);
                if part.is_empty() {
                    continue;
                }
                if index == 5 && part[0] != b'.' as u16 {
                    text.push(b'.' as u16);
                }
                text.extend_from_slice(&part);
                if index == 3 && !matches!(part.last(), Some(&0x5C) | Some(&0x2F)) {
                    text.push(b'\\' as u16);
                }
            }
            ok(secure_copy(memory, p(0), arg(1), &text, false, false) as u64)
        }
        "_mktemp_s" | "_wmktemp_s" => {
            let wide = name == "_wmktemp_s";
            let mut text = units_at(memory, arg(0), wide);
            let stamp = format!("{:06}", now().1 * 1000 + (state(ctx, |s| { s.seed = s.seed.wrapping_add(1); s.seed }) % 1000));
            let length = text.len();
            if length >= 6 {
                for (k, c) in stamp.bytes().rev().take(6).enumerate() {
                    if text[length - 1 - k] == b'X' as u16 {
                        text[length - 1 - k] = c as u16;
                    }
                }
            }
            write_units(memory, p(0), &text, wide, true);
            ok(0)
        }
        // ── Streams ──
        "fopen" | "_wfopen" | "_fsopen" | "_wfsopen" | "freopen" => {
            let wide = name.starts_with("_w");
            let path = path_arg(memory, arg(0), wide);
            let mode = path_arg(memory, arg(1), wide);
            let Some(flags) = mode_flags(&mode) else {
                set_errno(ctx, memory, EINVAL);
                return ok(0);
            };
            if name == "freopen" {
                if let Some(old) = stream(ctx, arg(2)) {
                    close_fd(ctx, old.fd);
                }
            }
            match open_path(ctx, &path, flags) {
                Ok(fd) => {
                    if name == "freopen" {
                        update_stream(ctx, arg(2), |s| *s = Stream { fd, ..Stream::default() });
                        ok(arg(2))
                    } else {
                        ok(new_stream(ctx, memory, fd) as u64)
                    }
                }
                Err(code) => {
                    set_errno(ctx, memory, code);
                    ok(0)
                }
            }
        }
        "_fdopen" => ok(new_stream(ctx, memory, p(0) as i32) as u64),
        "fclose" => match state(ctx, |s| s.streams.remove(&p(0))) {
            Some(stream) => {
                if stream.fd > 2 {
                    close_fd(ctx, stream.fd);
                }
                release(ctx, p(0));
                ok(0)
            }
            None => signed(-1),
        },
        "fflush" | "setvbuf" | "setbuf" => ok(0),
        "fwrite" => {
            let length = arg(1).saturating_mul(arg(2)) as usize;
            if length == 0 {
                return ok(0);
            }
            let bytes = memory.read_bytes(p(0), length).unwrap_or_default();
            match stream_write(ctx, arg(3), &bytes) {
                Some(_) => ok(arg(2)),
                None => ok(0),
            }
        }
        "fread" => {
            let (unit, items) = (arg(1), arg(2));
            if unit == 0 || items == 0 {
                return ok(0);
            }
            let bytes = stream_read(ctx, arg(3), (unit * items) as usize);
            let _ = memory.write_bytes(p(0), &bytes);
            ok(bytes.len() as u64 / unit)
        }
        "fgetc" | "fgetwc" => {
            let bytes = stream_read(ctx, arg(0), 1);
            match bytes.first() {
                Some(&b) if name == "fgetwc" => ok(unicode::ansi_to_wide(&[b]).first().copied().unwrap_or(b as u16) as u64),
                Some(&b) => ok(b as u64),
                None => ok(if name == "fgetwc" { 0xFFFF } else { u32::MAX as u64 }),
            }
        }
        "fgets" => {
            // fgets(buffer, size, stream)
            let mut line = Vec::new();
            while (line.len() as u64) + 1 < arg(1) {
                let byte = stream_read(ctx, arg(2), 1);
                let Some(&b) = byte.first() else { break };
                line.push(b);
                if b == b'\n' {
                    break;
                }
            }
            if line.is_empty() {
                return ok(0);
            }
            put_c(memory, p(0), &line);
            ok(arg(0))
        }
        "ungetc" | "ungetwc" => {
            let c = p(0);
            if c == u32::MAX || (name == "ungetwc" && c == 0xFFFF) {
                return ok(c as u64);
            }
            update_stream(ctx, arg(1), |s| {
                s.pushed.push(c as u8);
                s.eof = false;
            });
            ok(c as u64)
        }
        "fputc" | "fputwc" => {
            let bytes = if name == "fputwc" { unicode::wide_to_ansi(&[p(0) as u16]) } else { vec![p(0) as u8] };
            match stream_write(ctx, arg(1), &bytes) {
                Some(_) => ok(p(0) as u64),
                None => ok(if name == "fputwc" { 0xFFFF } else { u32::MAX as u64 }),
            }
        }
        "fputs" => {
            let text = cstr(memory, arg(0));
            match stream_write(ctx, arg(1), &text) {
                Some(_) => ok(0),
                None => signed(-1),
            }
        }
        "puts" => {
            let mut text = cstr(memory, arg(0));
            text.push(b'\n');
            fd_write(ctx, 1, &text);
            ok(0)
        }
        "putchar" => {
            fd_write(ctx, 1, &[p(0) as u8]);
            ok(p(0) as u64)
        }
        "perror" => {
            let prefix = cstr(memory, arg(0));
            let errno = static_block(ctx, memory, "errno", 8);
            let code = memory.read_u32(errno).unwrap_or(0);
            eprintln!("{}: {}", String::from_utf8_lossy(&prefix), error_text(code));
            ok(0)
        }
        "fseek" | "_fseeki64" => {
            let offset = if name == "fseek" { p(1) as i32 as i64 } else { arg(1) as i64 };
            match stream_seek(ctx, arg(0), offset, p(2)) {
                Some(_) => ok(0),
                None => signed(-1),
            }
        }
        "ftell" | "_ftelli64" => match stream_tell(ctx, arg(0)) {
            Some(position) => ok(position),
            None => signed(-1),
        },
        "fgetpos" => match stream_tell(ctx, arg(0)) {
            Some(position) => {
                let _ = memory.write_u64(p(1), position);
                ok(0)
            }
            None => signed(-1),
        },
        "fsetpos" => {
            let position = memory.read_u64(p(1)).unwrap_or(0);
            match stream_seek(ctx, arg(0), position as i64, 0) {
                Some(_) => ok(0),
                None => signed(-1),
            }
        }
        "rewind" => {
            stream_seek(ctx, arg(0), 0, 0);
            update_stream(ctx, arg(0), |s| s.error = false);
            ok(0)
        }
        "feof" => ok(u64::from(stream(ctx, arg(0)).is_some_and(|s| s.eof))),
        "ferror" => ok(u64::from(stream(ctx, arg(0)).is_some_and(|s| s.error))),
        "_fileno" => match stream(ctx, arg(0)) {
            Some(s) => ok(s.fd as u64),
            None => signed(-1),
        },
        "_get_stream_buffer_pointers" => {
            // Pointers to the stream's (always empty) buffer fields.
            let file = p(0);
            for (index, offset) in [(1usize, size), (2, 0), (3, 2 * size)] {
                if p(index) != 0 {
                    let _ = memory.write_ptr(p(index), (file + offset) as u64);
                }
            }
            ok(0)
        }
        // ── Formatted output and input ──
        "__stdio_common_vsprintf" | "__stdio_common_vswprintf" | "__stdio_common_vsprintf_s"
        | "__stdio_common_vswprintf_s" | "__stdio_common_vsnprintf_s" | "__stdio_common_vsnwprintf_s" => {
            let wide = name.contains("vsw") || name.contains("vsnw");
            let options = arg(0);
            let (buffer, capacity) = (p(1), arg(2));
            let snprintf_s = name.contains("vsn");
            let (max_count, fmt_at, va_at) = if snprintf_s { (arg(3), 4, 6) } else { (TRUNCATE, 3, 5) };
            let fmt = read_format(memory, arg(fmt_at), wide);
            let text = output_for(&format(memory, &fmt, p(va_at), wide, options & 4 != 0), wide);
            let n = text.len() as u64;
            if std::env::var_os("OTW_TRACE_PRINTF").is_some() {
                eprintln!("[printf] {name} cap {capacity} fmt {:?} -> {:?}", String::from_utf16_lossy(&fmt), String::from_utf16_lossy(&text));
            }
            if buffer == 0 {
                return ok(n);
            }
            if name.ends_with("printf_s") && !snprintf_s {
                if n >= capacity {
                    if capacity > 0 {
                        write_units(memory, buffer, &[], wide, true);
                    }
                    return signed(-1);
                }
                write_units(memory, buffer, &text, wide, true);
                return ok(n);
            }
            if snprintf_s {
                let limit = if max_count == TRUNCATE { capacity.saturating_sub(1) } else { max_count.min(capacity.saturating_sub(1)) };
                if n <= limit {
                    write_units(memory, buffer, &text, wide, true);
                    return ok(n);
                }
                if max_count == TRUNCATE || max_count < capacity {
                    write_units(memory, buffer, &text[..limit as usize], wide, true);
                    return signed(-1);
                }
                write_units(memory, buffer, &[], wide, true);
                return signed(-1);
            }
            if n < capacity {
                write_units(memory, buffer, &text, wide, true);
                return ok(n);
            }
            if options & 2 != 0 {
                // C99 snprintf: truncate, terminate, report the full length.
                if capacity > 0 {
                    write_units(memory, buffer, &text[..capacity as usize - 1], wide, true);
                }
                return ok(n);
            }
            write_units(memory, buffer, &text[..capacity as usize], wide, false);
            if n == capacity { ok(n) } else { signed(-1) }
        }
        "__stdio_common_vfprintf" | "__stdio_common_vfwprintf" | "__stdio_common_vfprintf_s"
        | "__stdio_common_vfwprintf_s" => {
            let wide = name.contains("vfw");
            let fmt = read_format(memory, arg(2), wide);
            let text = format(memory, &fmt, p(4), wide, arg(0) & 4 != 0);
            let bytes = unicode::wide_to_ansi(&text);
            if std::env::var_os("OTW_TRACE_PRINTF").is_some() {
                eprintln!("[printf] {name} fmt {:?} -> {:?}", String::from_utf16_lossy(&fmt), String::from_utf16_lossy(&text));
            }
            match stream_write(ctx, arg(1), &bytes) {
                Some(_) => ok(text.len() as u64),
                None => signed(-1),
            }
        }
        "__stdio_common_vsscanf" | "__stdio_common_vswscanf" => {
            let wide = name.contains("vsw");
            let mut input = units_at(memory, arg(1), wide);
            if arg(2) != TRUNCATE && (arg(2) as usize) < input.len() {
                input.truncate(arg(2) as usize);
            }
            let fmt = read_format(memory, arg(3), wide);
            let (assigned, _) = scan(memory, &input, &fmt, p(5), wide);
            signed(assigned as i64)
        }
        "__stdio_common_vfscanf" | "__stdio_common_vfwscanf" => {
            // Scan what is left of the stream, then give back the rest.
            let wide = name.contains("vfw");
            let Some(stream_state) = stream(ctx, arg(1)) else { return signed(-1) };
            let rest = stream_read(ctx, arg(1), 1 << 20);
            let input: Vec<u16> = rest.iter().map(|&b| b as u16).collect();
            let fmt = read_format(memory, arg(2), wide);
            let (assigned, used) = scan(memory, &input, &fmt, p(4), wide);
            let unused = (rest.len() - used.min(rest.len())) as i64;
            if let Some(Fd::File(handle)) = fd_of(ctx, stream_state.fd) {
                crt::seek_file(ctx, handle, -unused, 1);
            }
            update_stream(ctx, arg(1), |s| s.eof = unused == 0);
            signed(assigned as i64)
        }
        // ── C++ support the runtime exports ──
        "__std_exception_copy" => {
            // (from, to): { const char* what; bool owned }
            let what = memory.read_ptr(p(0)).unwrap_or(0);
            let owned = memory.read_bytes(p(0) + size, 1).and_then(|b| b.first().copied()).unwrap_or(0);
            if owned != 0 && what != 0 {
                let text = cstr(memory, what);
                let copy = alloc(ctx, text.len() as u64 + 1);
                put_c(memory, copy, &text);
                let _ = memory.write_ptr(p(1), copy as u64);
                let _ = memory.write_bytes(p(1) + size, &[1]);
            } else {
                let _ = memory.write_ptr(p(1), what);
                let _ = memory.write_bytes(p(1) + size, &[0]);
            }
            ok(0)
        }
        "__std_exception_destroy" => {
            let owned = memory.read_bytes(p(0) + size, 1).and_then(|b| b.first().copied()).unwrap_or(0);
            if owned != 0 {
                release(ctx, memory.read_ptr(p(0)).unwrap_or(0) as u32);
            }
            let _ = memory.write_ptr(p(0), 0);
            let _ = memory.write_bytes(p(0) + size, &[0]);
            ok(0)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            8
        }
    }

    #[test]
    fn printf_reads_a_64_bit_va_list() {
        let mut memory = Flat(vec![0; 512]);
        memory.write_bytes(100, b"Parent\0").unwrap();
        memory.write_bytes(120, b"hello\0").unwrap();
        // va_list: "Parent", 1 (long), "hello"
        memory.write_bytes(200, &100u64.to_le_bytes()).unwrap();
        memory.write_bytes(208, &1u64.to_le_bytes()).unwrap();
        memory.write_bytes(216, &120u64.to_le_bytes()).unwrap();
        let fmt: Vec<u16> = "[%s %ld]: %s|%5.2f".encode_utf16().collect();
        memory.write_bytes(224, &2.5f64.to_bits().to_le_bytes()).unwrap();
        let out = format(&mut memory, &fmt, 200, false, false);
        assert_eq!(String::from_utf16_lossy(&out), "[Parent 1]: hello| 2.50");
    }

    #[test]
    fn floats_format_like_the_c_library() {
        assert_eq!(format_float(1.5, b'f', Some(2), false), "1.50");
        assert_eq!(format_float(12345.678, b'e', None, false), "1.234568e+04");
        assert_eq!(format_float(0.0001, b'g', None, false), "0.0001");
        assert_eq!(format_float(1e-5, b'g', None, false), "1e-05");
        assert_eq!(format_float(100.0, b'g', None, false), "100");
        assert_eq!(format_float(f64::INFINITY, b'f', None, false), "inf");
    }

    #[test]
    fn numbers_parse_like_strtol_and_strtod() {
        let units = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        assert_eq!(parse_integer(&units("  -42xyz"), 10), (42, true, 5, false));
        assert_eq!(parse_integer(&units("0x1F"), 0), (31, false, 4, false));
        assert_eq!(parse_integer(&units("017"), 0), (15, false, 3, false));
        assert_eq!(parse_float(&units(" 3.25e2!")), (325.0, 7));
        assert_eq!(parse_float(&units("abc")).1, 0);
    }

    #[test]
    fn dates_convert_both_ways() {
        let tm = broken_down(951_782_400); // 2000-02-29 00:00:00
        assert_eq!(&tm[..8], &[0, 0, 0, 29, 1, 100, 2, 59]);
        assert_eq!(time_of(&tm), 951_782_400);
    }

    #[test]
    fn command_lines_split_like_the_c_library() {
        let units = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        let args = split_command_line(&units(r#"prog.exe "a b" c\"d e\\"#));
        let text: Vec<String> = args.iter().map(|a| String::from_utf16_lossy(a)).collect();
        assert_eq!(text, ["prog.exe", "a b", "c\"d", "e\\\\"]);
    }
}

/// Machine code for C library leaves that 64-bit programs call in hot
/// loops (Firefox's software renderer calls `floorf` per pixel): run by the
/// CPU instead of dispatched. The x64 convention: arguments in RCX, RDX,
/// R8 (XMM0 for floating point), the result in RAX or XMM0.
pub fn native_x64(name: &str) -> Option<&'static [u8]> {
    Some(match name {
        // roundss/roundsd xmm0, xmm0, mode (9 floor, 10 ceiling, 11
        // toward zero; inexact not raised); ret.
        "floorf" => &[0x66, 0x0F, 0x3A, 0x0A, 0xC0, 0x09, 0xC3],
        "ceilf" => &[0x66, 0x0F, 0x3A, 0x0A, 0xC0, 0x0A, 0xC3],
        "truncf" => &[0x66, 0x0F, 0x3A, 0x0A, 0xC0, 0x0B, 0xC3],
        "floor" => &[0x66, 0x0F, 0x3A, 0x0B, 0xC0, 0x09, 0xC3],
        "ceil" => &[0x66, 0x0F, 0x3A, 0x0B, 0xC0, 0x0A, 0xC3],
        "trunc" => &[0x66, 0x0F, 0x3A, 0x0B, 0xC0, 0x0B, 0xC3],
        // sqrtss/sqrtsd xmm0, xmm0; ret.
        "sqrtf" => &[0xF3, 0x0F, 0x51, 0xC0, 0xC3],
        "sqrt" => &[0xF2, 0x0F, 0x51, 0xC0, 0xC3],
        // strlen: rax = rcx; while [rax] != 0: rax++; return rax - rcx.
        "strlen" => &[
            0x48, 0x89, 0xC8, // mov rax, rcx
            0x80, 0x38, 0x00, // loop: cmp byte [rax], 0
            0x74, 0x05, // je done
            0x48, 0xFF, 0xC0, // inc rax
            0xEB, 0xF6, // jmp loop
            0x48, 0x29, 0xC8, // done: sub rax, rcx
            0xC3,
        ],
        // strcmp: the difference of the first differing bytes (unsigned).
        "strcmp" => &[
            0x0F, 0xB6, 0x01, // loop: movzx eax, byte [rcx]
            0x44, 0x0F, 0xB6, 0x02, // movzx r8d, byte [rdx]
            0x44, 0x29, 0xC0, // sub eax, r8d
            0x75, 0x0D, // jnz done
            0x45, 0x85, 0xC0, // test r8d, r8d
            0x74, 0x08, // jz done
            0x48, 0xFF, 0xC1, // inc rcx
            0x48, 0xFF, 0xC2, // inc rdx
            0xEB, 0xE7, // jmp loop
            0xC3, // done: ret
        ],
        // strncmp: as strcmp, for at most R8 bytes.
        "strncmp" => &[
            0x31, 0xC0, // xor eax, eax
            0x4D, 0x85, 0xC0, // loop: test r8, r8
            0x74, 0x1C, // jz done
            0x0F, 0xB6, 0x01, // movzx eax, byte [rcx]
            0x44, 0x0F, 0xB6, 0x0A, // movzx r9d, byte [rdx]
            0x44, 0x29, 0xC8, // sub eax, r9d
            0x75, 0x10, // jnz done
            0x45, 0x85, 0xC9, // test r9d, r9d
            0x74, 0x0B, // jz done
            0x48, 0xFF, 0xC1, // inc rcx
            0x48, 0xFF, 0xC2, // inc rdx
            0x49, 0xFF, 0xC8, // dec r8
            0xEB, 0xDF, // jmp loop
            0xC3, // done: ret
        ],
        _ => return None,
    })
}
