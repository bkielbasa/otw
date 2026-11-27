//! winget manifests: the per-package version list (`versionData.mszyml`)
//! and the merged per-version manifest that it points to.
//!
//! The merged manifest is one YAML document that combines winget's
//! version, installer and default-locale manifests. Installer fields may
//! appear at the top level as defaults for every entry of `Installers`.

use crate::error::{Error, Result};
use crate::version::Version;
use yaml_rust2::{Yaml, YamlLoader};

/// One entry of a package's version list.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionEntry {
    pub version: String,
    /// Path of the merged manifest, relative to the source's base URL.
    pub relative_path: String,
    /// SHA-256 of the merged manifest file.
    pub sha256: String,
}

/// Parses a decompressed `versionData.mszyml`. The result is sorted
/// newest first.
pub fn parse_version_data(text: &str) -> Result<Vec<VersionEntry>> {
    let docs =
        YamlLoader::load_from_str(text).map_err(|e| Error::format(format!("version data: {e}")))?;
    let doc = docs
        .first()
        .ok_or_else(|| Error::format("empty version data"))?;
    let list = doc["vD"]
        .as_vec()
        .ok_or_else(|| Error::format("version data has no vD list"))?;
    let mut out = Vec::new();
    for item in list {
        let (Some(version), Some(relative_path)) = (scalar(&item["v"]), scalar(&item["rP"])) else {
            return Err(Error::format("version data entry without v or rP"));
        };
        out.push(VersionEntry {
            version,
            relative_path,
            sha256: scalar(&item["s256H"]).unwrap_or_default(),
        });
    }
    out.sort_by(|a, b| Version::parse(&b.version).cmp(&Version::parse(&a.version)));
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arch {
    X86,
    X64,
    Arm,
    Arm64,
    Neutral,
    Other(String),
}

impl Arch {
    pub fn parse(s: &str) -> Arch {
        match s.to_ascii_lowercase().as_str() {
            "x86" | "i386" | "i686" => Arch::X86,
            "x64" | "amd64" | "x86_64" => Arch::X64,
            "arm" => Arch::Arm,
            "arm64" | "aarch64" => Arch::Arm64,
            "neutral" => Arch::Neutral,
            other => Arch::Other(other.to_string()),
        }
    }

    /// The architecture of the machine the program was built for.
    pub fn native() -> Arch {
        match std::env::consts::ARCH {
            "x86" => Arch::X86,
            "aarch64" => Arch::Arm64,
            "arm" => Arch::Arm,
            _ => Arch::X64,
        }
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Arch::X86 => "x86",
            Arch::X64 => "x64",
            Arch::Arm => "arm",
            Arch::Arm64 => "arm64",
            Arch::Neutral => "neutral",
            Arch::Other(s) => s,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Installer {
    pub architecture: Option<Arch>,
    /// winget's InstallerType, lower case: exe, inno, nullsoft, burn, msi,
    /// wix, msix, appx, zip, portable, pwa, font.
    pub installer_type: String,
    /// For zip installers, the type of what is inside.
    pub nested_installer_type: Option<String>,
    pub url: String,
    pub sha256: String,
    pub scope: Option<String>,
    pub locale: Option<String>,
    pub silent_switch: Option<String>,
}

impl Installer {
    pub fn arch_label(&self) -> String {
        self.architecture
            .as_ref()
            .map(Arch::to_string)
            .unwrap_or_else(|| "?".into())
    }

    pub fn type_label(&self) -> String {
        match &self.nested_installer_type {
            Some(n) => format!("{}/{}", self.installer_type, n),
            None => self.installer_type.clone(),
        }
    }

    /// The file name to save the installer as: the last part of the URL
    /// path, made safe for a Windows file system.
    pub fn file_name(&self, fallback: &str) -> String {
        let path = self.url.split(['?', '#']).next().unwrap_or("");
        let last = path.rsplit('/').next().unwrap_or("");
        let decoded = crate::util::percent_decode(last);
        let safe: String = decoded
            .chars()
            .map(|c| {
                if c.is_control() || "<>:\"/\\|?*".contains(c) {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let safe = safe.trim_matches(|c| c == '.' || c == ' ').to_string();
        if safe.is_empty() {
            fallback.to_string()
        } else {
            safe
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub name: String,
    pub publisher: String,
    pub moniker: Option<String>,
    pub short_description: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    pub release_notes_url: Option<String>,
    pub tags: Vec<String>,
    pub installers: Vec<Installer>,
}

/// The text of a scalar YAML value, whatever type YAML gave it.
fn scalar(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) | Yaml::Real(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn opt(y: &Yaml, key: &str) -> Option<String> {
    scalar(&y[key]).filter(|s| !s.trim().is_empty())
}

fn string_list(y: &Yaml) -> Vec<String> {
    y.as_vec()
        .map(|v| v.iter().filter_map(scalar).collect())
        .unwrap_or_default()
}

pub fn parse_manifest(text: &str) -> Result<Manifest> {
    let docs =
        YamlLoader::load_from_str(text).map_err(|e| Error::format(format!("manifest: {e}")))?;
    let root = docs
        .first()
        .ok_or_else(|| Error::format("empty manifest"))?;
    let id = opt(root, "PackageIdentifier")
        .ok_or_else(|| Error::format("manifest has no PackageIdentifier"))?;
    let version = opt(root, "PackageVersion")
        .ok_or_else(|| Error::format("manifest has no PackageVersion"))?;

    let switch = |y: &Yaml| {
        let s = &y["InstallerSwitches"];
        opt(s, "Silent").or_else(|| opt(s, "SilentWithProgress"))
    };
    let root_type = opt(root, "InstallerType");
    let root_nested = opt(root, "NestedInstallerType");
    let root_scope = opt(root, "Scope");
    let root_locale = opt(root, "InstallerLocale");
    let root_switch = switch(root);

    let mut installers = Vec::new();
    for item in root["Installers"]
        .as_vec()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let Some(url) = opt(item, "InstallerUrl") else {
            continue;
        };
        let installer_type = opt(item, "InstallerType")
            .or_else(|| root_type.clone())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let nested = if installer_type == "zip" {
            opt(item, "NestedInstallerType")
                .or_else(|| root_nested.clone())
                .map(|s| s.to_ascii_lowercase())
        } else {
            None
        };
        installers.push(Installer {
            architecture: opt(item, "Architecture").map(|a| Arch::parse(&a)),
            installer_type,
            nested_installer_type: nested,
            url,
            sha256: opt(item, "InstallerSha256").unwrap_or_default(),
            scope: opt(item, "Scope").or_else(|| root_scope.clone()),
            locale: opt(item, "InstallerLocale").or_else(|| root_locale.clone()),
            silent_switch: switch(item).or_else(|| root_switch.clone()),
        });
    }

    Ok(Manifest {
        name: opt(root, "PackageName").unwrap_or_else(|| id.clone()),
        publisher: opt(root, "Publisher").unwrap_or_default(),
        moniker: opt(root, "Moniker"),
        short_description: opt(root, "ShortDescription"),
        description: opt(root, "Description"),
        license: opt(root, "License"),
        homepage: opt(root, "PackageUrl").or_else(|| opt(root, "PublisherUrl")),
        release_notes_url: opt(root, "ReleaseNotesUrl"),
        tags: string_list(&root["Tags"]),
        installers,
        id,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn version_data_is_sorted_newest_first() {
        let text = "sV: 1.0\nvD:\n- v: 1.9\n  rP: a/1.9\n  s256H: aa\n- v: 1.10\n  rP: a/1.10\n  s256H: bb\n- v: 2\n  rP: a/2\n";
        let list = parse_version_data(text).unwrap();
        let versions: Vec<&str> = list.iter().map(|e| e.version.as_str()).collect();
        assert_eq!(versions, ["2", "1.10", "1.9"]);
        assert_eq!(list[1].relative_path, "a/1.10");
        assert_eq!(list[1].sha256, "bb");
        assert_eq!(list[0].sha256, "");
    }

    #[test]
    fn version_data_from_the_real_index() {
        let raw = std::fs::read(format!(
            "{}/tests/fixtures/7zip.versionData.mszyml",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let text = String::from_utf8(crate::mszip::decompress_buffer(&raw).unwrap()).unwrap();
        let list = parse_version_data(&text).unwrap();
        assert_eq!(list.len(), 24);
        assert_eq!(list[0].version, "26.06");
        assert_eq!(list[0].relative_path, "manifests/7/7zip/7zip/26.06/67fd");
        assert_eq!(list.last().unwrap().version, "16.04");
        // 26.00 must not lose its trailing zero, YAML reads it as a float.
        assert!(list.iter().any(|e| e.version == "26.00"));
    }

    #[test]
    fn version_data_errors() {
        assert!(parse_version_data("sV: 1.0\n").is_err());
        assert!(parse_version_data("vD:\n- rP: x\n").is_err());
        assert!(parse_version_data("vD: [").is_err());
    }

    #[test]
    fn parses_7zip() {
        let m = parse_manifest(&fixture("7zip.7zip-26.06.yaml")).unwrap();
        assert_eq!(m.id, "7zip.7zip");
        assert_eq!(m.version, "26.06");
        assert_eq!(m.name, "7-Zip");
        assert_eq!(m.publisher, "Igor Pavlov");
        assert_eq!(m.moniker.as_deref(), Some("7zip"));
        assert_eq!(m.license.as_deref(), Some("LGPL-2.1"));
        assert_eq!(
            m.homepage.as_deref(),
            Some("https://7-zip.org/download.html")
        );
        assert_eq!(
            m.short_description.as_deref(),
            Some("A file archiver with a high compression ratio.")
        );
        assert!(m.tags.contains(&"zip".to_string()));
        assert_eq!(m.installers.len(), 6);
        let x64 = &m.installers[1];
        assert_eq!(x64.architecture, Some(Arch::X64));
        assert_eq!(x64.installer_type, "exe");
        assert_eq!(
            x64.url,
            "https://github.com/ip7z/7zip/releases/download/26.06/7z2606-x64.exe"
        );
        assert_eq!(
            x64.sha256,
            "9B697F65ECC2D9B0152F5EE96D72DDE522A7827690349D9307935DACF40F42EC"
        );
        assert_eq!(x64.silent_switch.as_deref(), Some("/S"));
        assert_eq!(m.installers[5].installer_type, "wix");
        // The scope is only given at the top level.
        assert!(m
            .installers
            .iter()
            .all(|i| i.scope.as_deref() == Some("machine")));
    }

    #[test]
    fn top_level_installer_defaults_apply() {
        let m = parse_manifest(&fixture("rustup-1.29.3.yaml")).unwrap();
        assert_eq!(m.installers.len(), 3);
        assert!(m.installers.iter().all(|i| i.installer_type == "exe"));
        assert!(m
            .installers
            .iter()
            .all(|i| i.silent_switch.as_deref() == Some("-y -q")));
        let m = parse_manifest(&fixture("windowsterminal-1.25.yaml")).unwrap();
        assert!(m.installers.iter().all(|i| i.installer_type == "msix"));
    }

    #[test]
    fn zip_installers_keep_the_nested_type() {
        let m = parse_manifest(&fixture("notepadpp-8.9.9.1.yaml")).unwrap();
        let zips: Vec<&Installer> = m
            .installers
            .iter()
            .filter(|i| i.installer_type == "zip")
            .collect();
        assert_eq!(zips.len(), 3);
        assert!(zips
            .iter()
            .all(|i| i.nested_installer_type.as_deref() == Some("portable")));
        assert_eq!(zips[0].type_label(), "zip/portable");
        assert_eq!(m.installers[0].installer_type, "nullsoft");
        assert_eq!(m.version, "8.9.9.1");
    }

    #[test]
    fn localized_strings_are_not_used_for_the_default() {
        let m = parse_manifest(&fixture("windowsterminal-1.25.yaml")).unwrap();
        assert_eq!(m.name, "Windows Terminal");
        assert_eq!(m.publisher, "Microsoft Corporation");
    }

    #[test]
    fn hex_escapes_in_strings() {
        let m = parse_manifest(
            "PackageIdentifier: A.B\nPackageVersion: 1.0\nPublisher: \"Caf\\xE9\"\nInstallers: []\n",
        )
        .unwrap();
        assert_eq!(m.publisher, "Caf\u{e9}");
        assert_eq!(m.version, "1.0");
        assert_eq!(m.name, "A.B");
    }

    #[test]
    fn manifest_errors() {
        assert!(parse_manifest("PackageVersion: 1\n").is_err());
        assert!(parse_manifest("PackageIdentifier: a\n").is_err());
        assert!(parse_manifest("").is_err());
    }

    #[test]
    fn installers_without_url_are_skipped() {
        let m = parse_manifest(
            "PackageIdentifier: A.B\nPackageVersion: 1\nInstallers:\n- Architecture: x64\n- Architecture: x86\n  InstallerUrl: https://e.com/a.exe\n",
        )
        .unwrap();
        assert_eq!(m.installers.len(), 1);
        assert_eq!(m.installers[0].architecture, Some(Arch::X86));
        assert_eq!(m.installers[0].installer_type, "");
    }

    #[test]
    fn file_names() {
        let i = |url: &str| Installer {
            url: url.into(),
            ..Default::default()
        };
        assert_eq!(
            i("https://e.com/dl/7z2606-x64.exe").file_name("f"),
            "7z2606-x64.exe"
        );
        assert_eq!(
            i("https://e.com/dl/setup.exe?token=1#x").file_name("f"),
            "setup.exe"
        );
        assert_eq!(
            i("https://e.com/dl/My%20App%3A1.msi").file_name("f"),
            "My App_1.msi"
        );
        assert_eq!(
            i("https://e.com/download/").file_name("fallback.bin"),
            "fallback.bin"
        );
        assert_eq!(
            i("https://e.com/..").file_name("fallback.bin"),
            "fallback.bin"
        );
        assert_eq!(
            i("https://e.com/a%2F..%2Fb.exe").file_name("f"),
            "a_.._b.exe"
        );
    }

    #[test]
    fn arch_parsing() {
        assert_eq!(Arch::parse("X64"), Arch::X64);
        assert_eq!(Arch::parse("amd64"), Arch::X64);
        assert_eq!(Arch::parse("arm64"), Arch::Arm64);
        assert_eq!(Arch::parse("neutral"), Arch::Neutral);
        assert_eq!(Arch::parse("mips").to_string(), "mips");
    }
}
