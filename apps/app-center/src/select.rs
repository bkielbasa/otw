//! Choosing which of a manifest's installers to download.
//!
//! Installers are put in three groups by what OTW can run:
//!
//! 1. programs that run on their own: `nullsoft`, `inno`, `exe`, `burn`,
//!    `portable`, and `zip` archives holding one of those;
//! 2. Windows Installer packages (`msi`, `wix`), which need msiexec;
//! 3. packaged apps (`msix`, `appx`, `msstore`, `pwa`, `font`, and zips of
//!    them), which need the Windows app deployment stack. These are only
//!    chosen when asked for.
//!
//! The group decides first, then the architecture (the requested one, then
//! one that runs on it: x86 on x64, then neutral), then the type within the
//! group, then a matching or missing locale. The manifest order breaks ties.

use crate::manifest::{Arch, Installer};

#[derive(Debug, Clone, PartialEq)]
pub struct Preferences {
    pub arch: Arch,
    /// Accept packaged-app installers too.
    pub allow_packaged: bool,
    /// Preferred installer locale, e.g. "en-US".
    pub locale: Option<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            arch: Arch::X64,
            allow_packaged: false,
            locale: Some("en-US".into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Support {
    Packaged = 0,
    NeedsMsiexec = 1,
    Runs = 2,
}

/// What OTW can do with an installer type, and how much a type is preferred
/// within its group (higher is better).
pub fn support(installer: &Installer) -> (Support, u8) {
    let base = |t: &str| -> (Support, u8) {
        match t {
            "nullsoft" | "inno" => (Support::Runs, 5),
            "exe" | "burn" => (Support::Runs, 4),
            "portable" => (Support::Runs, 3),
            "wix" | "msi" => (Support::NeedsMsiexec, 2),
            "" => (Support::Runs, 1),
            _ => (Support::Packaged, 0),
        }
    };
    if installer.installer_type == "zip" {
        // An archive is one step more work than the same thing unpacked.
        let (s, rank) = base(installer.nested_installer_type.as_deref().unwrap_or(""));
        (s, rank.saturating_sub(3))
    } else {
        base(&installer.installer_type)
    }
}

/// How well an installer's architecture suits `wanted`; None if it cannot run.
pub fn arch_rank(installer: Option<&Arch>, wanted: &Arch) -> Option<u8> {
    let Some(have) = installer else {
        return Some(1);
    };
    if have == wanted {
        return Some(3);
    }
    match (wanted, have) {
        (_, Arch::Neutral) => Some(1),
        (Arch::X64, Arch::X86) => Some(2),
        (Arch::Arm64, Arch::X64) => Some(2),
        (Arch::Arm64, Arch::X86 | Arch::Arm) => Some(1),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct Choice<'a> {
    pub installer: &'a Installer,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NoChoice {
    /// The manifest lists no installers at all.
    NoInstallers,
    /// No installer runs on the requested architecture.
    NoArch { available: Vec<String> },
    /// Only packaged-app installers would fit.
    OnlyPackaged { available: Vec<String> },
}

impl std::fmt::Display for NoChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoChoice::NoInstallers => write!(f, "the manifest lists no installers"),
            NoChoice::NoArch { available } => write!(
                f,
                "no installer for this architecture (available: {})",
                available.join(", ")
            ),
            NoChoice::OnlyPackaged { available } => write!(
                f,
                "only packaged-app installers ({}), which OTW cannot install; pass --any-type to download one anyway",
                available.join(", ")
            ),
        }
    }
}

/// Ranks every installer that can be used; best first.
pub fn rank<'a>(installers: &'a [Installer], prefs: &Preferences) -> Vec<&'a Installer> {
    let mut scored: Vec<(usize, (Support, u8, u8, u8), &Installer)> = installers
        .iter()
        .enumerate()
        .filter_map(|(i, inst)| {
            let arch = arch_rank(inst.architecture.as_ref(), &prefs.arch)?;
            let (group, type_rank) = support(inst);
            if group == Support::Packaged && !prefs.allow_packaged {
                return None;
            }
            let locale = match (&inst.locale, &prefs.locale) {
                (Some(have), Some(want)) if have.eq_ignore_ascii_case(want) => 2,
                (None, _) => 1,
                _ => 0,
            };
            Some((i, (group, arch, type_rank, locale), inst))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(_, _, inst)| inst).collect()
}

pub fn choose<'a>(
    installers: &'a [Installer],
    prefs: &Preferences,
) -> Result<Choice<'a>, NoChoice> {
    if installers.is_empty() {
        return Err(NoChoice::NoInstallers);
    }
    let describe = |list: &mut dyn Iterator<Item = &Installer>| {
        let mut v: Vec<String> = list
            .map(|i| format!("{} {}", i.arch_label(), i.type_label()))
            .collect();
        v.dedup();
        v
    };
    let Some(best) = rank(installers, prefs).into_iter().next() else {
        let runnable_arch: Vec<&Installer> = installers
            .iter()
            .filter(|i| arch_rank(i.architecture.as_ref(), &prefs.arch).is_some())
            .collect();
        return Err(if runnable_arch.is_empty() {
            NoChoice::NoArch {
                available: describe(&mut installers.iter()),
            }
        } else {
            NoChoice::OnlyPackaged {
                available: describe(&mut runnable_arch.into_iter()),
            }
        });
    };

    let mut reason = Vec::new();
    match &best.architecture {
        Some(a) if *a == prefs.arch => {
            reason.push(format!("matches the requested {a} architecture"))
        }
        Some(Arch::Neutral) | None => reason.push("architecture-neutral".to_string()),
        Some(a) => reason.push(format!("{a} build; no {} build is usable", prefs.arch)),
    }
    let (group, _) = support(best);
    reason.push(match group {
        Support::Runs => format!("{} installers run directly", best.type_label()),
        Support::NeedsMsiexec => format!(
            "{} needs msiexec, but no installer that runs directly fits",
            best.type_label()
        ),
        Support::Packaged => format!(
            "{} is a packaged app (allowed by --any-type)",
            best.type_label()
        ),
    });
    let skipped = installers
        .iter()
        .filter(|i| support(i).0 == Support::Packaged && !prefs.allow_packaged)
        .count();
    if skipped > 0 {
        reason.push(format!("skipped {skipped} packaged-app installer(s)"));
    }
    Ok(Choice {
        installer: best,
        reason: reason.join("; "),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    fn inst(arch: Option<Arch>, ty: &str, nested: Option<&str>, url: &str) -> Installer {
        Installer {
            architecture: arch,
            installer_type: ty.into(),
            nested_installer_type: nested.map(Into::into),
            url: url.into(),
            ..Default::default()
        }
    }

    fn prefs(arch: Arch) -> Preferences {
        Preferences {
            arch,
            ..Default::default()
        }
    }

    fn fixture(name: &str) -> crate::manifest::Manifest {
        let text = std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        parse_manifest(&text).unwrap()
    }

    #[test]
    fn seven_zip_x64_picks_the_exe() {
        let m = fixture("7zip.7zip-26.06.yaml");
        let c = choose(&m.installers, &prefs(Arch::X64)).unwrap();
        assert!(c.installer.url.ends_with("7z2606-x64.exe"));
        assert!(
            c.reason.contains("matches the requested x64"),
            "{}",
            c.reason
        );
        let c = choose(&m.installers, &prefs(Arch::X86)).unwrap();
        assert!(c.installer.url.ends_with("7z2606.exe"));
    }

    #[test]
    fn notepad_plus_plus_prefers_nullsoft_over_msi_and_zip() {
        let m = fixture("notepadpp-8.9.9.1.yaml");
        let ranked = rank(&m.installers, &prefs(Arch::X64));
        let urls: Vec<&str> = ranked
            .iter()
            .map(|i| i.url.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(
            urls,
            [
                "npp.8.9.9.1.Installer.x64.exe",
                "npp.8.9.9.1.portable.x64.zip",
                "npp.8.9.9.1.Installer.exe",
                "npp.8.9.9.1.portable.zip",
                "npp.8.9.9.1.Installer.x64.msi",
            ]
        );
    }

    #[test]
    fn msix_only_needs_a_flag() {
        let m = fixture("windowsterminal-1.25.yaml");
        match choose(&m.installers, &prefs(Arch::X64)) {
            Err(NoChoice::OnlyPackaged { available }) => {
                assert_eq!(available, ["x86 msix", "x64 msix"])
            }
            r => panic!("unexpected {r:?}"),
        }
        let p = Preferences {
            allow_packaged: true,
            ..prefs(Arch::X64)
        };
        let c = choose(&m.installers, &p).unwrap();
        assert_eq!(c.installer.architecture, Some(Arch::X64));
        assert!(c.reason.contains("packaged app"));
    }

    #[test]
    fn runnable_x86_beats_x64_msi() {
        let list = [
            inst(Some(Arch::X64), "msi", None, "a.msi"),
            inst(Some(Arch::X86), "inno", None, "b.exe"),
        ];
        let c = choose(&list, &prefs(Arch::X64)).unwrap();
        assert_eq!(c.installer.url, "b.exe");
        assert!(c.reason.contains("x86 build"), "{}", c.reason);
    }

    #[test]
    fn msi_is_used_when_nothing_else_fits() {
        let list = [
            inst(Some(Arch::X64), "msix", None, "a.msix"),
            inst(Some(Arch::X64), "wix", None, "b.msi"),
        ];
        let c = choose(&list, &prefs(Arch::X64)).unwrap();
        assert_eq!(c.installer.url, "b.msi");
        assert!(c.reason.contains("msiexec"));
        assert!(c.reason.contains("skipped 1 packaged-app"));
    }

    #[test]
    fn x86_request_does_not_take_x64() {
        let list = [inst(Some(Arch::X64), "exe", None, "a.exe")];
        match choose(&list, &prefs(Arch::X86)) {
            Err(NoChoice::NoArch { available }) => assert_eq!(available, ["x64 exe"]),
            r => panic!("unexpected {r:?}"),
        }
    }

    #[test]
    fn arm_builds_are_never_chosen_for_x64() {
        let list = [
            inst(Some(Arch::Arm64), "nullsoft", None, "arm.exe"),
            inst(Some(Arch::Neutral), "zip", Some("portable"), "any.zip"),
        ];
        let c = choose(&list, &prefs(Arch::X64)).unwrap();
        assert_eq!(c.installer.url, "any.zip");
        assert!(c.reason.contains("neutral"));
    }

    #[test]
    fn zip_of_msix_is_packaged() {
        let i = inst(Some(Arch::X64), "zip", Some("msix"), "a.zip");
        assert_eq!(support(&i).0, Support::Packaged);
        let i = inst(Some(Arch::X64), "zip", Some("exe"), "a.zip");
        assert_eq!(support(&i), (Support::Runs, 1));
    }

    #[test]
    fn locale_breaks_ties() {
        let mut de = inst(Some(Arch::X64), "exe", None, "de.exe");
        de.locale = Some("de-DE".into());
        let mut en = inst(Some(Arch::X64), "exe", None, "en.exe");
        en.locale = Some("en-US".into());
        let list = [de, en];
        assert_eq!(
            choose(&list, &prefs(Arch::X64)).unwrap().installer.url,
            "en.exe"
        );
    }

    #[test]
    fn manifest_order_breaks_remaining_ties() {
        let list = [
            inst(Some(Arch::X64), "exe", None, "first.exe"),
            inst(Some(Arch::X64), "exe", None, "second.exe"),
        ];
        assert_eq!(
            choose(&list, &prefs(Arch::X64)).unwrap().installer.url,
            "first.exe"
        );
    }

    #[test]
    fn empty_list() {
        assert_eq!(
            choose(&[], &prefs(Arch::X64)).unwrap_err(),
            NoChoice::NoInstallers
        );
    }

    #[test]
    fn arch_ranks() {
        assert_eq!(arch_rank(Some(&Arch::X64), &Arch::X64), Some(3));
        assert_eq!(arch_rank(Some(&Arch::X86), &Arch::X64), Some(2));
        assert_eq!(arch_rank(Some(&Arch::Neutral), &Arch::X86), Some(1));
        assert_eq!(arch_rank(None, &Arch::X86), Some(1));
        assert_eq!(arch_rank(Some(&Arch::X64), &Arch::X86), None);
        assert_eq!(arch_rank(Some(&Arch::Arm64), &Arch::X64), None);
    }
}
