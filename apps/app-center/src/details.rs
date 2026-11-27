//! Everything a front end shows about one package: its manifest, its
//! versions and the installer [`select`](crate::select) would download,
//! fetched in one go and owned, so it can be sent between threads.

use crate::error::Result;
use crate::index::Package;
use crate::manifest::{Installer, Manifest, VersionEntry};
use crate::select::{self, NoChoice, Preferences};
use crate::source::{Fetch, Source};

/// The installer that would be downloaded: its position in
/// `manifest.installers` and why it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub index: usize,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Details {
    pub manifest: Manifest,
    /// Every version, newest first.
    pub versions: Vec<String>,
    pub choice: std::result::Result<Chosen, NoChoice>,
}

impl Details {
    /// Puts together the details of `manifest`, choosing its installer.
    pub fn new(manifest: Manifest, versions: &[VersionEntry], prefs: &Preferences) -> Details {
        let choice = select::choose(&manifest.installers, prefs).map(|c| Chosen {
            index: manifest
                .installers
                .iter()
                .position(|i| std::ptr::eq(i, c.installer))
                .unwrap_or(0),
            reason: c.reason,
        });
        Details {
            versions: versions.iter().map(|v| v.version.clone()).collect(),
            manifest,
            choice,
        }
    }

    /// The installer that would be downloaded, if there is one.
    pub fn installer(&self) -> Option<&Installer> {
        self.choice
            .as_ref()
            .ok()
            .and_then(|c| self.manifest.installers.get(c.index))
    }

    /// The name to save the installer as when its URL has none.
    pub fn fallback_file_name(&self) -> String {
        format!("{}-{}.bin", self.manifest.id, self.manifest.version)
    }
}

/// Fetches the version list and the manifest of `version` (or the newest)
/// of `package`: two small requests.
pub fn fetch<F: Fetch>(
    src: &Source<F>,
    package: &Package,
    version: Option<&str>,
    prefs: &Preferences,
) -> Result<Details> {
    let versions = src.versions(package)?;
    let entry = Source::<F>::pick_version(&versions, version)?;
    let manifest = src.manifest(entry)?;
    Ok(Details::new(manifest, &versions, prefs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Arch;
    use crate::source::tests::{fake_source, temp_dir};

    #[test]
    fn fetches_7zip() {
        let cache = temp_dir("details");
        let src = fake_source(&cache);
        let (index, _) = src.index(false).unwrap();
        let d = fetch(
            &src,
            index.get("7zip.7zip").unwrap(),
            None,
            &Preferences::default(),
        )
        .unwrap();
        assert_eq!(d.manifest.name, "7-Zip");
        assert_eq!(d.versions[0], "26.06");
        assert!(d.versions.len() > 10);
        let inst = d.installer().unwrap();
        assert_eq!(inst.architecture, Some(Arch::X64));
        assert!(inst.url.ends_with("-x64.exe"), "{}", inst.url);
        assert!(d.choice.as_ref().unwrap().reason.contains("x64"));
        assert_eq!(d.fallback_file_name(), "7zip.7zip-26.06.bin");
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let cache = temp_dir("details-404");
        let src = fake_source(&cache);
        let (index, _) = src.index(false).unwrap();
        // Only 7-Zip's files are served.
        assert!(fetch(
            &src,
            index.get("Mozilla.Firefox").unwrap(),
            None,
            &Preferences::default()
        )
        .is_err());
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    fn packaged_only_has_no_installer() {
        let m = Manifest {
            id: "X.Y".into(),
            version: "1".into(),
            installers: vec![Installer {
                installer_type: "msix".into(),
                architecture: Some(Arch::X64),
                ..Default::default()
            }],
            ..Default::default()
        };
        let d = Details::new(m, &[], &Preferences::default());
        assert!(d.installer().is_none());
        assert!(matches!(d.choice, Err(NoChoice::OnlyPackaged { .. })));
    }
}
