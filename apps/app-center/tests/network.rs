//! Tests against the real winget source. They need the network, so they
//! are ignored by default: `cargo test -- --ignored`.

use app_center::select::{choose, Preferences};
use app_center::source::{Http, Source, DEFAULT_SOURCE};

fn source() -> Source<Http> {
    let cache = std::env::temp_dir().join(format!("app-center-net-{}", std::process::id()));
    Source::new(DEFAULT_SOURCE, Http::new(), cache)
}

#[test]
#[ignore = "needs the network"]
fn real_index_finds_7zip_and_its_installer() {
    let src = source();
    let (index, _) = src.index(true).unwrap();
    assert!(index.packages.len() > 1000);
    let pkg = app_center::resolve(&index, "7zip").unwrap();
    assert_eq!(pkg.id, "7zip.7zip");
    let versions = src.versions(pkg).unwrap();
    let manifest = src.manifest(&versions[0]).unwrap();
    assert_eq!(manifest.id, "7zip.7zip");
    let choice = choose(&manifest.installers, &Preferences::default()).unwrap();
    assert!(choice.installer.url.ends_with(".exe"));
    let _ = std::fs::remove_dir_all(&src.cache_dir);
}
