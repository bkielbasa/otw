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

#[test]
#[ignore = "needs the network"]
fn featured_apps_exist_and_have_details() {
    let src = source();
    let (index, _) = src.index(true).unwrap();
    let missing: Vec<&str> = app_center::ui::app::FEATURED
        .iter()
        .copied()
        .filter(|id| index.get(id).is_none())
        .collect();
    let present = app_center::ui::app::FEATURED.len() - missing.len();
    assert!(
        present >= 10,
        "only {present} featured apps; missing {missing:?}"
    );
    for id in ["Mozilla.Firefox", "IrfanSkiljan.IrfanView"] {
        let pkg = index.get(id).unwrap();
        let d = app_center::details::fetch(&src, pkg, None, &Preferences::default()).unwrap();
        assert!(d.installer().is_some(), "{id}: {:?}", d.choice);
        assert!(!d.versions.is_empty());
    }
    let _ = std::fs::remove_dir_all(&src.cache_dir);
}
