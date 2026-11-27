//! Command-line parsing for the `app-center` program.

use crate::error::{Error, Result};
use crate::manifest::Arch;
use std::path::PathBuf;

pub const USAGE: &str = "\
App Center: find and download Windows applications from the winget community repository.

Usage:
  app-center search <query> [--limit N]
  app-center show <id> [--version V] [--arch x64|x86] [--versions]
  app-center download <id> [--version V] [--arch x64|x86] [--dir PATH] [--any-type]
  app-center update

<id> is a package identifier such as 7zip.7zip, or a name or moniker that
only one package has.

Options:
  --version V     a specific version (default: the newest)
  --arch A        installer architecture: x64 (default) or x86
  --dir PATH      where to save the installer (default: the current directory)
  --any-type      also accept MSIX/AppX installers, which OTW cannot install
  --versions      list every version (show)
  --limit N       number of results (search, default 20)
  --source URL    the winget source (default https://cdn.winget.microsoft.com/cache)
  --cache DIR     where to keep the index (default: APP_CENTER_CACHE or the user cache)
  --refresh       download the index again even if the cached copy is recent
";

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Search {
        query: String,
        limit: usize,
    },
    Show {
        id: String,
        version: Option<String>,
        arch: Arch,
        all_versions: bool,
    },
    Download {
        id: String,
        version: Option<String>,
        arch: Arch,
        dir: PathBuf,
        any_type: bool,
    },
    Update,
    Help,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub command: Command,
    pub source: Option<String>,
    pub cache: Option<PathBuf>,
    pub refresh: bool,
}

fn parse_arch(s: &str) -> Result<Arch> {
    match Arch::parse(s) {
        a @ (Arch::X64 | Arch::X86 | Arch::Arm64) => Ok(a),
        _ => Err(Error::Usage(format!(
            "unknown architecture {s:?}; use x64 or x86"
        ))),
    }
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Options> {
    let mut positional = Vec::new();
    let mut version = None;
    let mut arch = Arch::X64;
    let mut dir = PathBuf::from(".");
    let mut any_type = false;
    let mut all_versions = false;
    let mut limit = 20;
    let mut source = None;
    let mut cache = None;
    let mut refresh = false;
    let mut help = false;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        // Accept both "--opt value" and "--opt=value".
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| -> Result<String> {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| Error::Usage(format!("{what} needs a value")))
        };
        match name.as_str() {
            "-h" | "--help" | "help" if positional.is_empty() || name != "help" => help = true,
            "--version" | "-v" => version = Some(value("--version")?),
            "--arch" | "-a" => arch = parse_arch(&value("--arch")?)?,
            "--dir" | "-d" => dir = PathBuf::from(value("--dir")?),
            "--any-type" => any_type = true,
            "--versions" => all_versions = true,
            "--limit" | "-n" => {
                let v = value("--limit")?;
                limit = v
                    .parse()
                    .map_err(|_| Error::Usage(format!("--limit wants a number, not {v:?}")))?;
            }
            "--source" => source = Some(value("--source")?),
            "--cache" => cache = Some(PathBuf::from(value("--cache")?)),
            "--refresh" => refresh = true,
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(Error::Usage(format!("unknown option {s}")));
            }
            _ => positional.push(arg),
        }
    }

    let command = if help || positional.is_empty() {
        Command::Help
    } else {
        let rest = || positional[1..].join(" ");
        let need = |what: &str| -> Result<String> {
            let r = rest();
            if r.is_empty() {
                Err(Error::Usage(format!("{} needs {what}", positional[0])))
            } else {
                Ok(r)
            }
        };
        match positional[0].as_str() {
            "search" | "find" => Command::Search {
                query: need("a query")?,
                limit,
            },
            "show" | "info" => Command::Show {
                id: need("a package id")?,
                version,
                arch,
                all_versions,
            },
            "download" | "get" => Command::Download {
                id: need("a package id")?,
                version,
                arch,
                dir,
                any_type,
            },
            "update" => Command::Update,
            other => return Err(Error::Usage(format!("unknown command {other:?}"))),
        }
    };
    Ok(Options {
        command,
        source,
        cache,
        refresh,
    })
}

/// Formats a byte count for people.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Options> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn search() {
        let o = p(&["search", "notepad++"]).unwrap();
        assert_eq!(
            o.command,
            Command::Search {
                query: "notepad++".into(),
                limit: 20
            }
        );
        let o = p(&["search", "visual", "studio", "--limit", "5"]).unwrap();
        assert_eq!(
            o.command,
            Command::Search {
                query: "visual studio".into(),
                limit: 5
            }
        );
        assert!(p(&["search"]).is_err());
        assert!(p(&["search", "x", "--limit", "many"]).is_err());
    }

    #[test]
    fn download_with_options() {
        let o = p(&[
            "download",
            "7zip.7zip",
            "--version=24.09",
            "--arch",
            "x86",
            "--dir",
            "/tmp/x",
            "--any-type",
        ])
        .unwrap();
        assert_eq!(
            o.command,
            Command::Download {
                id: "7zip.7zip".into(),
                version: Some("24.09".into()),
                arch: Arch::X86,
                dir: "/tmp/x".into(),
                any_type: true,
            }
        );
    }

    #[test]
    fn show_defaults() {
        let o = p(&["show", "7zip.7zip"]).unwrap();
        assert_eq!(
            o.command,
            Command::Show {
                id: "7zip.7zip".into(),
                version: None,
                arch: Arch::X64,
                all_versions: false
            }
        );
        assert!(!o.refresh);
        assert_eq!(o.source, None);
    }

    #[test]
    fn global_options() {
        let o = p(&[
            "--source",
            "https://x/cache",
            "--cache",
            "/c",
            "--refresh",
            "update",
        ])
        .unwrap();
        assert_eq!(o.command, Command::Update);
        assert_eq!(o.source.as_deref(), Some("https://x/cache"));
        assert_eq!(o.cache, Some(PathBuf::from("/c")));
        assert!(o.refresh);
    }

    #[test]
    fn help_and_errors() {
        assert_eq!(p(&[]).unwrap().command, Command::Help);
        assert_eq!(p(&["--help"]).unwrap().command, Command::Help);
        assert_eq!(p(&["help"]).unwrap().command, Command::Help);
        assert!(p(&["frobnicate"]).is_err());
        assert!(p(&["show", "x", "--bogus"]).is_err());
        assert!(p(&["download", "x", "--arch", "mips"]).is_err());
        assert!(p(&["download", "x", "--dir"]).is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1_610_612_736), "1.5 GiB");
    }
}
