//! The `app-center` command-line front end.

use app_center::cli::{self, human_size, Command, Options};
use app_center::index::Index;
use app_center::manifest::{Manifest, VersionEntry};
use app_center::select::{self, Preferences};
use app_center::source::{default_cache_dir, Http, IndexOrigin, Source, DEFAULT_SOURCE};
use app_center::{download, resolve, search, Error, Result};
use std::io::Write;
use std::time::Instant;

/// Allocations from big blocks of the program's own (see Cargo.toml), not
/// a system heap call each.
#[cfg(windows)]
#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

fn main() {
    let opts = match cli::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("app-center: {e}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    if let Err(e) = run(opts) {
        eprintln!("app-center: {e}");
        std::process::exit(match e {
            Error::Usage(_) => 2,
            _ => 1,
        });
    }
}

fn load_index(src: &Source<Http>, refresh: bool) -> Result<Index> {
    let started = Instant::now();
    let (index, origin) = src.index(refresh)?;
    if origin == IndexOrigin::Downloaded {
        eprintln!(
            "Downloaded the package index ({} packages) in {:.1}s.",
            index.packages.len(),
            started.elapsed().as_secs_f32()
        );
    }
    Ok(index)
}

fn run(opts: Options) -> Result<()> {
    let src = Source::new(
        opts.source.as_deref().unwrap_or(DEFAULT_SOURCE),
        Http::new(),
        opts.cache.clone().unwrap_or_else(default_cache_dir),
    );
    match opts.command {
        Command::Help => {
            print!("{}", cli::USAGE);
            Ok(())
        }
        Command::Update => {
            let index = load_index(&src, true)?;
            println!(
                "Index updated: {} packages, cached in {}.",
                index.packages.len(),
                src.cache_dir.display()
            );
            Ok(())
        }
        Command::Search { query, limit } => {
            let index = load_index(&src, opts.refresh)?;
            let hits = search::search(&index, &query);
            if hits.is_empty() {
                println!("No package matches {query:?}.");
                return Ok(());
            }
            let shown = &hits[..hits.len().min(limit)];
            let w_name = shown
                .iter()
                .map(|h| h.package.name.chars().count().min(40))
                .max()
                .unwrap_or(4)
                .max(4);
            let w_id = shown
                .iter()
                .map(|h| h.package.id.len())
                .max()
                .unwrap_or(2)
                .max(2);
            let w_ver = shown
                .iter()
                .map(|h| h.package.latest_version.len())
                .max()
                .unwrap_or(7)
                .max(7);
            println!(
                "{:<w_name$}  {:<w_id$}  {:<w_ver$}  Match",
                "Name", "Id", "Version"
            );
            println!("{}", "-".repeat(w_name + w_id + w_ver + 13));
            for h in shown {
                let name: String = h.package.name.chars().take(40).collect();
                println!(
                    "{:<w_name$}  {:<w_id$}  {:<w_ver$}  {}",
                    name,
                    h.package.id,
                    h.package.latest_version,
                    h.kind.label()
                );
            }
            if hits.len() > shown.len() {
                println!("... and {} more (use --limit).", hits.len() - shown.len());
            }
            Ok(())
        }
        Command::Show {
            id,
            version,
            arch,
            all_versions,
        } => {
            let index = load_index(&src, opts.refresh)?;
            let package = resolve(&index, &id)?;
            let versions = src.versions(package)?;
            let entry = Source::<Http>::pick_version(&versions, version.as_deref())?;
            let manifest = src.manifest(entry)?;
            print_manifest(&manifest);
            print_installers(
                &manifest,
                &Preferences {
                    arch,
                    ..Default::default()
                },
            );
            print_versions(&versions, all_versions);
            Ok(())
        }
        Command::Download {
            id,
            version,
            arch,
            dir,
            any_type,
        } => {
            let index = load_index(&src, opts.refresh)?;
            let package = resolve(&index, &id)?;
            let versions = src.versions(package)?;
            let entry = Source::<Http>::pick_version(&versions, version.as_deref())?;
            let manifest = src.manifest(entry)?;
            let prefs = Preferences {
                arch,
                allow_packaged: any_type,
                ..Default::default()
            };
            let choice = select::choose(&manifest.installers, &prefs).map_err(|e| {
                Error::NotFound(format!("{} {}: {e}", manifest.id, manifest.version))
            })?;
            let inst = choice.installer;
            println!("{} {} ({})", manifest.name, manifest.version, manifest.id);
            println!(
                "Installer: {} {}  {}",
                inst.arch_label(),
                inst.type_label(),
                inst.url
            );
            println!("Chosen because: {}", choice.reason);
            let fallback = format!("{}-{}.bin", manifest.id, manifest.version);
            let started = Instant::now();
            let mut last = Instant::now();
            let done = download::download(&src.fetch, inst, &dir, &fallback, &mut |n, total| {
                if last.elapsed().as_millis() >= 250 || Some(n) == total {
                    last = Instant::now();
                    match total {
                        Some(t) if t > 0 => eprint!(
                            "\r  {} / {} ({}%)   ",
                            human_size(n),
                            human_size(t),
                            n * 100 / t
                        ),
                        _ => eprint!("\r  {}   ", human_size(n)),
                    }
                    let _ = std::io::stderr().flush();
                }
            })?;
            eprintln!();
            println!(
                "Saved {} ({} in {:.1}s)",
                done.path.display(),
                human_size(done.size),
                started.elapsed().as_secs_f32()
            );
            println!("SHA-256 verified: {}", done.sha256);
            if let Some(sw) = &inst.silent_switch {
                println!("Silent install switch: {sw}");
            }
            Ok(())
        }
    }
}

fn print_manifest(m: &Manifest) {
    println!("{} {}", m.name, m.version);
    println!("  {:<15}{}", "Id:", m.id);
    let field = |label: &str, v: Option<&str>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            println!("  {label:<15}{v}");
        }
    };
    field("Publisher:", Some(&m.publisher));
    field("Moniker:", m.moniker.as_deref());
    field("License:", m.license.as_deref());
    field("Homepage:", m.homepage.as_deref());
    field("Release notes:", m.release_notes_url.as_deref());
    field("Summary:", m.short_description.as_deref());
    if !m.tags.is_empty() {
        println!("  {:<15}{}", "Tags:", m.tags.join(", "));
    }
    if let Some(d) = &m.description {
        println!();
        for line in d.lines() {
            println!("  {line}");
        }
    }
}

fn print_installers(m: &Manifest, prefs: &Preferences) {
    println!("\nInstallers:");
    let chosen = select::choose(&m.installers, prefs);
    for inst in &m.installers {
        let mark = match &chosen {
            Ok(c) if std::ptr::eq(c.installer, inst) => "*",
            _ => " ",
        };
        let (support, _) = select::support(inst);
        let note = match support {
            select::Support::Runs => "",
            select::Support::NeedsMsiexec => "  (needs msiexec)",
            select::Support::Packaged => "  (packaged app: not supported)",
        };
        println!(
            " {mark} {:<8} {:<15} {}{note}",
            inst.arch_label(),
            inst.type_label(),
            inst.url
        );
    }
    match chosen {
        Ok(c) => println!("  * would be downloaded for {}: {}", prefs.arch, c.reason),
        Err(e) => println!("  Nothing to download for {}: {e}", prefs.arch),
    }
}

fn print_versions(versions: &[VersionEntry], all: bool) {
    let n = if all {
        versions.len()
    } else {
        versions.len().min(10)
    };
    let list: Vec<&str> = versions[..n].iter().map(|v| v.version.as_str()).collect();
    print!("\nVersions ({}): {}", versions.len(), list.join(", "));
    if n < versions.len() {
        print!(", ... (--versions lists all)");
    }
    println!();
}
