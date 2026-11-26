//! The process environment: the variables Windows sets up (the Windows
//! folder, the temporary folder, the program files folder, and for 64-bit
//! programs the NT ones such as `OS` and `USERPROFILE`), and those a
//! program sets. Host variables named `OTW_ENV_<NAME>` add `<NAME>`, to
//! pass settings to a guest (e.g. `OTW_ENV_MOZ_CRASHREPORTER_DISABLE=1`).

#[allow(unused_imports)]
use crate::platform as std;
#[allow(unused_imports)]
use crate::platform::prelude::*;

use crate::win32::ApiContext;

#[derive(Debug, Default)]
pub struct State {
    vars: Option<Vec<(String, String)>>,
}

fn defaults(x64: bool) -> Vec<(String, String)> {
    let mut vars: Vec<(&str, &str)> = vec![
        ("COMSPEC", "C:\\COMMAND.COM"),
        ("PATH", "C:\\WINDOWS;C:\\WINDOWS\\COMMAND"),
        ("PROMPT", "$p$g"),
        ("TEMP", "C:\\WINDOWS\\TEMP"),
        ("TMP", "C:\\WINDOWS\\TEMP"),
        ("windir", "C:\\WINDOWS"),
    ];
    if x64 {
        vars.extend([
            ("ALLUSERSPROFILE", "C:\\WINDOWS\\All Users"),
            ("APPDATA", "C:\\WINDOWS\\Application Data"),
            ("COMPUTERNAME", "OTW"),
            ("HOMEDRIVE", "C:"),
            ("HOMEPATH", "\\WINDOWS"),
            ("LOCALAPPDATA", "C:\\WINDOWS\\Local Settings\\Application Data"),
            ("NUMBER_OF_PROCESSORS", "1"),
            ("OS", "Windows_NT"),
            ("PROCESSOR_ARCHITECTURE", "AMD64"),
            ("ProgramData", "C:\\WINDOWS\\All Users"),
            ("ProgramFiles", "C:\\Program Files"),
            ("SystemDrive", "C:"),
            ("SystemRoot", "C:\\WINDOWS"),
            ("USERNAME", "User"),
            ("USERPROFILE", "C:\\WINDOWS"),
        ]);
    }
    let mut vars: Vec<(String, String)> = vars.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for (key, value) in std::env::vars() {
        if let Some(name) = key.strip_prefix("OTW_ENV_") {
            vars.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
            vars.push((name.to_string(), value));
        }
    }
    vars
}

fn with<R>(ctx: &ApiContext, f: impl FnOnce(&mut Vec<(String, String)>) -> R) -> R {
    let x64 = ctx.shared.borrow().x64;
    let mut shared = ctx.shared.borrow_mut();
    let vars = shared.env.vars.get_or_insert_with(|| defaults(x64));
    f(vars)
}

/// A variable's value (names ignore case).
pub fn get(ctx: &ApiContext, name: &str) -> Option<String> {
    with(ctx, |vars| vars.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()))
}

/// Set a variable, or remove it with `None`.
pub fn set(ctx: &ApiContext, name: &str, value: Option<&str>) {
    with(ctx, |vars| {
        vars.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        if let Some(value) = value {
            vars.push((name.to_string(), value.to_string()));
        }
    });
}

/// The environment block: `name=value` strings, sorted by name.
pub fn block(ctx: &ApiContext) -> Vec<String> {
    let mut vars = with(ctx, |vars| vars.clone());
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    vars.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// Replace `%NAME%` references with their values.
pub fn expand(ctx: &ApiContext, text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[start..]);
            return out;
        };
        match get(ctx, &after[..end]) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..start + end + 2]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Replace the whole environment (a child given one by its parent).
pub fn set_all(state: &mut State, block: &[String]) {
    let vars = block
        .iter()
        .filter_map(|entry| {
            // Entries such as `=C:=C:\` start with the separator.
            let split = entry.char_indices().skip(1).find(|&(_, c)| c == '=').map(|(i, _)| i)?;
            Some((entry[..split].to_string(), entry[split + 1..].to_string()))
        })
        .collect();
    state.vars = Some(vars);
}
