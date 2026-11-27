# App Center

Find and download Windows applications listed in winget's community
repository ([microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs)),
so that a user of OTW can get software without a browser.

```
app-center search <query> [--limit N]
app-center show <id> [--version V] [--arch x64|x86] [--versions]
app-center download <id> [--version V] [--arch x64|x86] [--dir PATH] [--any-type]
app-center update
```

`<id>` is a package identifier (`7zip.7zip`, any case) or a moniker or name
that only one package has (`7zip`, `notepad++`). Global options: `--source
URL`, `--cache DIR`, `--refresh`.

```
$ app-center download 7zip --dir /tmp/dl
7-Zip 26.06 (7zip.7zip)
Installer: x64 exe  https://github.com/ip7z/7zip/releases/download/26.06/7z2606-x64.exe
Chosen because: matches the requested x64 architecture; exe installers run directly
Saved /tmp/dl/7z2606-x64.exe (1.6 MiB in 0.7s)
SHA-256 verified: 9b697f65ecc2d9b0152f5ee96d72dde522a7827690349d9307935dacf40f42ec
Silent install switch: /S
```

## The graphical App Center

`app-center-gui.exe` is the same thing as a window: a search box, a list
of apps (popular ones before anything is typed), and a details panel with
the description, licence, homepage, versions and the installer that would
be chosen and why. **Download** fetches it in the background with a
progress bar (size, speed, time left), checks its SHA-256 and saves it to
the Downloads folder (`FOLDERID_Downloads`, else `C:\My Documents\Downloads`);
then **Run installer** and **Show in folder** appear. The look follows
OTW's shell: dark, flat, one blue accent.

Keys: typing searches (after a short pause; Enter searches at once), Up,
Down, Page Up/Down and Ctrl+Home/End move through the list, Esc clears the
search, F5 updates the index, Ctrl+V pastes.

It is raw Win32 through `windows-sys`, using only classic USER32/GDI32
calls that OTW implements: one custom-painted, double-buffered window
(`CreateCompatibleDC`/`BitBlt`, `FillRect`, `RoundRect`, `Ellipse`,
`TextOutW`, `GetTextExtentPoint32W`), timers, and `PostMessageW` from
worker threads. Text wrapping, ellipsis, the search box's editing and caret
are done in Rust rather than by `DrawText` or an EDIT control. Opening
files uses `ShellExecuteW`. On other systems the binary just says that it
needs Windows.

All the state and what is shown lives in the library's `ui` module, which
has no Win32 in it and is unit-tested on Linux; the binary
(`src/bin/app-center-gui/`) only paints it and feeds it input.

## Where the data comes from

winget's default source, `winget`, is a static file tree on
`https://cdn.winget.microsoft.com/cache`. App Center reads the same files the
winget client reads (the "source2" format, index schema 2.x):

| File | What it is |
|------|------------|
| `source2.msix` (≈4 MB) | An MSIX package, i.e. a ZIP64 archive. `Public/index.db` inside is an SQLite database (≈9 MB) with one row per package: id, name, moniker, latest version, and the SHA-256 of its version list; plus tags, commands, product codes, normalized names and publishers. |
| `packages/<id>/<first 8 hex digits of that hash>/versionData.mszyml` | The package's versions, as YAML compressed with the Windows Compression API's MSZIP format. Each entry has the version, the path of its manifest (`rP`) and the manifest's SHA-256 (`s256H`). |
| `manifests/<letter>/<publisher>/<package>/<version>/<hash>` | The merged YAML manifest of one version: metadata plus the `Installers` list with `InstallerUrl`, `InstallerSha256`, `Architecture` and `InstallerType`. |

So a search is local (the index is cached for a day in `APP_CENTER_CACHE`,
`%LOCALAPPDATA%\AppCenter` or `~/.cache/app-center`), and `show`/`download`
cost two small requests (≈1 KB and ≈5 KB) before the installer itself.

The chain of hashes is checked end to end: the version list must match the
hash in the index, the manifest the hash in the version list, and the
installer the `InstallerSha256` in the manifest. An installer is written as
`<name>.part` and renamed only after its hash matched; on a mismatch it is
deleted. An installer without a hash is not downloaded.

### Alternatives considered

* **The older `source.msix` (index schema 1.x).** Also an SQLite database,
  but it holds every version of every package in normalized tables
  (the package is ≈22 MB against ≈4 MB for source2). source2 is
  smaller, is what current winget uses, and gives a hash per manifest.
* **Reading `microsoft/winget-pkgs` on GitHub** (raw.githubusercontent.com
  for manifests, the API or a clone for listing). Manifests there are split
  into three or more files per version, listing the repository needs the
  GitHub API (60 requests an hour without a token) or a clone of a
  repository with hundreds of thousands of files, and there is no search
  index. It is the fallback if the CDN format changes.
* **A winget REST source** (the protocol private sources use). The
  community repository does not offer one.

### Trade-offs

* The source2 layout and the `.mszyml` container are not documented by
  Microsoft; they were worked out from the files themselves and match what
  the open-source winget client does. If Microsoft changes them, the
  readers in `src/mszip.rs` and `src/index.rs` fail with a clear error
  (`index schema version ... is not supported`, `bad MSZIP header`).
* The signature of `source2.msix` is not checked; trust rests on HTTPS to
  `cdn.winget.microsoft.com` and the hash chain above.
* Only the default locale of a manifest is shown, and the index has no
  descriptions, so search uses ids, names, monikers, tags and commands.

## Choosing an installer

A manifest usually lists several installers. They are grouped by what OTW
can run, and the group decides first:

1. **runs directly**: `nullsoft`, `inno`, `exe`, `burn`, `portable`, and
   `zip` archives holding one of those;
2. **needs msiexec**: `msi`, `wix` (OTW has no msiexec yet);
3. **packaged apps**: `msix`, `appx`, `msstore`, `pwa`, `font` and zips of
   them. These need the Windows app deployment stack and are skipped unless
   `--any-type` is given.

Then the architecture: the requested one (default x64), then one that runs
on it (x86 on x64), then neutral; arm and arm64 builds are never chosen for
x86 or x64. Then the type (an NSIS or Inno Setup installer before a generic
exe, before a portable exe, before a zip), then a matching or missing
locale, then the manifest's order. `show` marks the chosen installer and
`download` prints why it was chosen.

## Layout

The library (`src/lib.rs`) does all the work; `src/main.rs` is a thin CLI
and `src/bin/app-center-gui/` a thin Win32 front end.

| Module | |
|--------|---|
| `source` | the source's URLs, the `Fetch` trait (HTTP or a fake), index caching |
| `zip` | minimal ZIP/ZIP64 reader for `source2.msix` |
| `sqlite` | minimal read-only SQLite file-format reader (tables, WITHOUT ROWID tables, overflow pages) |
| `index` | packages, tags and commands from `index.db` |
| `mszip` | `.mszyml` (Compression API MSZIP buffer) decompression |
| `manifest` | version lists and merged manifests |
| `version` | winget's version ordering |
| `search` | ranking packages against a query |
| `select` | installer selection |
| `download`, `hash` | streaming download with SHA-256 verification, cancellable |
| `details` | a package's manifest, versions and chosen installer in one owned value |
| `cli` | argument parsing |
| `ui::app` | the GUI's state machine: input and worker events in, effects and view models out |
| `ui::layout`, `ui::scroll`, `ui::geom` | window layout, list selection and scrolling, hit testing |
| `ui::text_input`, `ui::format`, `ui::theme` | the search box, user-facing text (progress, errors, wrapping), colours and fonts |

## Dependencies

All pure Rust except `ring` (rustls' crypto, which has C and assembly):
`ureq` with `rustls` and the compiled-in Mozilla root store (`webpki-roots`;
no OpenSSL, no reliance on an OS certificate store, which OTW does not
have), `miniz_oxide` (deflate), `sha2` and `yaml-rust2`. SQLite and ZIP are
read by small modules in this crate instead of `rusqlite` (bundled C SQLite)
and `zip`, to keep cross-compiling for Windows free of a C toolchain except
for ring.

## Building and testing

```
cargo build --release
cargo test                    # offline, uses tests/fixtures
cargo test -- --ignored       # also the tests that use the real CDN
```

The crate has its own `[workspace]` so it is not part of the `otw-vm`
package in the repository root.

`tests/fixtures/` holds real files from the CDN (7-Zip's version list;
manifests of 7-Zip, Notepad++, Rustup and Windows Terminal), a cut-down
`index.db` made by `make_index_db.sh` (512-byte pages and synthetic rows, so
that the tables are multi-level B-trees and one row uses overflow pages),
and small ZIP/MSZIP files made by `make_small_fixtures.py`.

### Windows

The goal is to run App Center inside OTW. It builds for
`x86_64-pc-windows-gnullvm` with llvm-mingw (`ring` needs its C compiler):

```
CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER=x86_64-w64-mingw32-clang \
CC_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-clang \
AR_x86_64_pc_windows_gnullvm=llvm-ar \
cargo build --release --target x86_64-pc-windows-gnullvm
```

which gives `app-center.exe` and `app-center-gui.exe`. The code has no
Unix-only parts (the cache directory uses `%LOCALAPPDATA%` on Windows).
