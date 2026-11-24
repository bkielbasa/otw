//! The system is layered; each layer may use only the layers below it:
//!
//! 1. `kernel`  — processes, CPU, scheduling, memory, loading, exceptions
//! 2. `win32`   — the Win32 environment subsystem
//! 3. `vm`      — a program: kernel process + Win32 system calls
//! 4. `session` — several programs on one desktop
//! 5. `shell`   — the desktop shell
//! 6. `host`    — the host window
//!
//! This test reads the sources and fails on any reference upwards.

use std::{fs, path::Path};

const LAYERS: &[&str] = &["kernel", "win32", "vm", "session", "shell", "host"];

fn sources(path: &Path, out: &mut Vec<(String, String)>) {
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            sources(&entry.unwrap().path(), out);
        }
    } else if path.extension().is_some_and(|e| e == "rs") {
        out.push((
            path.display().to_string(),
            fs::read_to_string(path).unwrap(),
        ));
    }
}

/// The layers a source file refers to through `crate::` paths, including
/// grouped imports (`use crate::{a::b, c}`).
fn referenced_layers(text: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("crate::") {
        rest = &rest[at + "crate::".len()..];
        let heads: Vec<&str> = if let Some(group) = rest.strip_prefix('{') {
            // Top-level items of the group.
            let mut depth = 0;
            let mut items = vec![];
            let mut start = 0;
            for (index, ch) in group.char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' if depth == 0 => {
                        items.push(&group[start..index]);
                        break;
                    }
                    '}' => depth -= 1,
                    ',' if depth == 0 => {
                        items.push(&group[start..index]);
                        start = index + 1;
                    }
                    _ => {}
                }
            }
            items
        } else {
            vec![rest]
        };
        for head in heads {
            let name: String = head
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if let Some(layer) = LAYERS.iter().find(|layer| **layer == name) {
                found.push(*layer);
            }
        }
    }
    found
}

#[test]
fn layers_only_use_the_layers_below_them() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for (index, layer) in LAYERS.iter().enumerate() {
        let mut files = Vec::new();
        let dir = root.join(layer);
        if dir.is_dir() {
            sources(&dir, &mut files);
        } else {
            sources(&root.join(format!("{layer}.rs")), &mut files);
        }
        assert!(!files.is_empty(), "layer {layer} has sources");
        for (path, text) in files {
            for used in referenced_layers(&text) {
                let used_index = LAYERS.iter().position(|l| *l == used).unwrap();
                if used_index > index {
                    violations.push(format!("{path}: {layer} uses {used}"));
                }
            }
        }
    }
    violations.sort();
    violations.dedup();
    assert!(
        violations.is_empty(),
        "upward references:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_kernel_does_not_know_win32_api_names() {
    // The kernel reaches API functions only through the system call
    // interface, so no Win32 function or DLL names appear in it.
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/kernel"),
        &mut files,
    );
    for (path, text) in files {
        for name in [
            "GetMessageA",
            "__CxxFrameHandler",
            "OTW_DYNAMIC",
            "ApiContext",
            "SharedState",
        ] {
            assert!(!text.contains(name), "{path} mentions {name}");
        }
    }
}
