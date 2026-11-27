//! The App Center window's state and what it shows, without any Win32.
//!
//! The window feeds [`App`] user input (typing, selection, button
//! actions, timers) and [`Event`]s from its worker threads; [`App`]
//! answers with [`Effect`]s for the window to carry out (start a timer,
//! fetch details on a thread, open a file) and describes what to paint
//! through its view functions ([`App::row`], [`App::details_view`],
//! [`App::status_line`], ...).

use super::format::{self, Message};
use super::scroll::{ListState, Scroll};
use super::text_input::TextInput;
use crate::details::Details;
use crate::download::Downloaded;
use crate::error::{Error, Result};
use crate::index::{Index, Package};
use crate::manifest::Installer;
use crate::search;
use crate::select::{self, Preferences, Support};
use crate::source::{IndexOrigin, IndexProgress};
use std::collections::HashMap;
use std::path::PathBuf;

/// How long typing must pause before the search runs.
pub const SEARCH_DEBOUNCE_MS: u32 = 220;
/// How long a row must stay selected before its details are fetched, so
/// that scrolling through the list with the keyboard does not fire a
/// request per row.
pub const DETAILS_DEBOUNCE_MS: u32 = 250;
/// The most results listed.
pub const MAX_RESULTS: usize = 500;

/// Shown before anything is searched, in this order, when the index has
/// them.
pub const FEATURED: &[&str] = &[
    "Mozilla.Firefox",
    "7zip.7zip",
    "IrfanSkiljan.IrfanView",
    "Notepad++.Notepad++",
    "VideoLAN.VLC",
    "Audacity.Audacity",
    "GIMP.GIMP.3",
    "KeePassXCTeam.KeePassXC",
    "SumatraPDF.SumatraPDF",
    "TheDocumentFoundation.LibreOffice",
    "Git.Git",
    "Python.Python.3.13",
    "Rustlang.Rustup",
    "dail8859.NotepadNext",
    "WinSCP.WinSCP",
    "PuTTY.PuTTY",
];

/// The window's timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timer {
    Search,
    Details,
}

/// What the worker threads report.
#[derive(Debug)]
pub enum Event {
    IndexProgress(IndexProgress),
    IndexLoaded(Result<(Index, IndexOrigin)>),
    DetailsLoaded {
        id: String,
        result: Result<Details>,
    },
    DownloadProgress {
        id: String,
        done: u64,
        total: Option<u64>,
    },
    DownloadFinished {
        id: String,
        result: Result<Downloaded>,
    },
}

/// What the window has to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Load the index on a worker thread (downloading it if needed, or
    /// always when `refresh`), reporting [`Event::IndexProgress`] and then
    /// [`Event::IndexLoaded`].
    LoadIndex { refresh: bool },
    /// (Re)start a one-shot timer; when it fires call [`App::timer`].
    StartTimer(Timer, u32),
    /// Fetch [`Details`] on a worker thread; report
    /// [`Event::DetailsLoaded`].
    FetchDetails { package: Package },
    /// Download on a worker thread; report [`Event::DownloadProgress`] and
    /// [`Event::DownloadFinished`].
    Download {
        id: String,
        installer: Installer,
        dir: PathBuf,
        fallback_name: String,
    },
    /// Ask the running download to stop.
    CancelDownload,
    /// Open a downloaded file (ShellExecute "open").
    Open(PathBuf),
    /// Show a downloaded file in its folder.
    ShowInFolder(PathBuf),
    /// Open a web page.
    OpenUrl(String),
}

/// What a button or link does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    Download,
    Cancel,
    Open,
    ShowInFolder,
    OpenUrl(String),
    RetryDetails,
    RefreshIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IndexState {
    Loading(Option<IndexProgress>),
    Ready {
        packages: usize,
        origin: IndexOrigin,
    },
    Failed(Message),
}

#[derive(Debug, Clone, PartialEq)]
enum DetailState {
    Loading,
    Ready(Box<Details>),
    Failed(Message),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveDownload {
    pub id: String,
    pub name: String,
    pub done: u64,
    pub total: Option<u64>,
    pub started_ms: u64,
    pub cancelling: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Finished {
    Saved(Downloaded),
    Failed(Message),
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusLine {
    pub kind: Kind,
    pub text: String,
}

/// One row of the results list.
#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub id: String,
    pub selected: bool,
    pub downloaded: bool,
    pub downloading: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProgressView {
    /// None for "busy, amount unknown".
    pub fraction: Option<f32>,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Secondary,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ButtonView {
    pub label: String,
    pub action: Action,
    pub style: ButtonStyle,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub kind: Kind,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub label: String,
    pub value: String,
    pub link: Option<Action>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PackageView {
    pub name: String,
    /// "Mozilla · 139.0"
    pub subtitle: String,
    pub summary: Option<String>,
    /// Details are still being fetched.
    pub loading: bool,
    pub buttons: Vec<ButtonView>,
    pub progress: Option<ProgressView>,
    pub notice: Option<Notice>,
    pub description: Option<String>,
    pub facts: Vec<Fact>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DetailsView {
    /// Nothing selected, or no index yet.
    Placeholder {
        title: String,
        detail: String,
        progress: Option<ProgressView>,
        button: Option<ButtonView>,
    },
    Package(Box<PackageView>),
}

pub struct App {
    index: Option<Index>,
    index_state: IndexState,
    /// A refresh is running while an older index stays usable.
    refreshing: bool,
    pub search: TextInput,
    /// The query the results are for; None while showing featured apps.
    searched: Option<String>,
    /// Positions in `index.packages`.
    results: Vec<usize>,
    pub list: ListState,
    details: HashMap<String, DetailState>,
    pub details_scroll: Scroll,
    download: Option<ActiveDownload>,
    finished: HashMap<String, Finished>,
    status: Option<StatusLine>,
    prefs: Preferences,
    download_dir: PathBuf,
    now_ms: u64,
}

fn position(index: &Index, p: &Package) -> usize {
    (p as *const Package as usize - index.packages.as_ptr() as usize)
        / std::mem::size_of::<Package>().max(1)
}

impl App {
    /// A new window's state, and the effects to start it: loading the
    /// index.
    pub fn new(download_dir: PathBuf, prefs: Preferences) -> (App, Vec<Effect>) {
        let app = App {
            index: None,
            index_state: IndexState::Loading(None),
            refreshing: false,
            search: TextInput::new(),
            searched: None,
            results: Vec::new(),
            list: ListState::new(super::layout::ROW_HEIGHT),
            details: HashMap::new(),
            details_scroll: Scroll::default(),
            download: None,
            finished: HashMap::new(),
            status: None,
            prefs,
            download_dir,
            now_ms: 0,
        };
        (app, vec![Effect::LoadIndex { refresh: false }])
    }

    /// Tells the state the time, in milliseconds since some fixed start.
    pub fn set_time(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }

    pub fn index_state(&self) -> &IndexState {
        &self.index_state
    }

    /// A refresh of the index is running.
    pub fn refreshing(&self) -> bool {
        self.refreshing
    }

    /// The installer preferences details are fetched with.
    pub fn prefs(&self) -> &Preferences {
        &self.prefs
    }

    pub fn download_dir(&self) -> &std::path::Path {
        &self.download_dir
    }

    pub fn active_download(&self) -> Option<&ActiveDownload> {
        self.download.as_ref()
    }

    /// Something is in progress (for animating busy indicators).
    pub fn busy(&self) -> bool {
        matches!(self.index_state, IndexState::Loading(_))
            || self.refreshing
            || self.download.is_some()
            || self
                .selected_package()
                .is_some_and(|p| matches!(self.details.get(&p.id), Some(DetailState::Loading)))
    }

    fn package(&self, row: usize) -> Option<&Package> {
        let i = *self.results.get(row)?;
        self.index.as_ref()?.packages.get(i)
    }

    pub fn selected_package(&self) -> Option<&Package> {
        self.package(self.list.selected?)
    }

    fn selected_details(&self) -> Option<&Details> {
        match self.details.get(&self.selected_package()?.id)? {
            DetailState::Ready(d) => Some(d),
            _ => None,
        }
    }

    // ── Search ──────────────────────────────────────────────────────────

    /// The search text changed: search once typing pauses.
    pub fn query_edited(&mut self) -> Vec<Effect> {
        vec![Effect::StartTimer(Timer::Search, SEARCH_DEBOUNCE_MS)]
    }

    /// Enter in the search box: search now.
    pub fn submit(&mut self) -> Vec<Effect> {
        self.run_search(true)
    }

    /// Escape: clears the search and goes back to the featured apps.
    pub fn clear_search(&mut self) -> Vec<Effect> {
        if self.search.is_empty() {
            return Vec::new();
        }
        self.search.set("");
        self.run_search(false)
    }

    /// Searches for the text in the box (or lists featured apps when it is
    /// empty). Unless `force`, nothing happens when the results are already
    /// for this text.
    fn run_search(&mut self, force: bool) -> Vec<Effect> {
        let Some(index) = &self.index else {
            return Vec::new();
        };
        let query = self.search.text().trim().to_string();
        let wanted = (!query.is_empty()).then(|| query.clone());
        if !force && wanted == self.searched && !self.results.is_empty() {
            return Vec::new();
        }
        let results: Vec<usize> = if query.is_empty() {
            FEATURED
                .iter()
                .filter_map(|id| index.packages.iter().position(|p| p.id == *id))
                .collect()
        } else {
            search::search(index, &query)
                .iter()
                .take(MAX_RESULTS)
                .map(|h| position(index, h.package))
                .collect()
        };
        let previous = self.selected_package().map(|p| p.id.clone());
        self.searched = wanted;
        self.results = results;
        self.list.reset(self.results.len());
        // Keep the selected app selected if it is still listed.
        let keep = previous.and_then(|id| {
            (0..self.results.len()).find(|&r| self.package(r).is_some_and(|p| p.id == id))
        });
        if self.results.is_empty() {
            self.details_scroll.scroll_to(0);
            return Vec::new();
        }
        self.list.select(keep.unwrap_or(0));
        self.selection_changed()
    }

    // ── Selection ───────────────────────────────────────────────────────

    fn selection_changed(&mut self) -> Vec<Effect> {
        self.details_scroll.scroll_to(0);
        match self.selected_package() {
            Some(p) if !self.details.contains_key(&p.id) => {
                vec![Effect::StartTimer(Timer::Details, DETAILS_DEBOUNCE_MS)]
            }
            _ => Vec::new(),
        }
    }

    /// A click on a row.
    pub fn select_row(&mut self, row: usize) -> Vec<Effect> {
        if row >= self.results.len() || self.list.selected == Some(row) {
            return Vec::new();
        }
        self.list.select(row);
        self.selection_changed()
    }

    /// Up/Down (`delta` ±1), Page Up/Down (±page), Home/End (first/last).
    pub fn move_selection(&mut self, delta: isize) -> Vec<Effect> {
        if self.list.move_by(delta) {
            self.selection_changed()
        } else {
            Vec::new()
        }
    }

    pub fn page_selection(&mut self, pages: isize) -> Vec<Effect> {
        if self.list.page_by(pages) {
            self.selection_changed()
        } else {
            Vec::new()
        }
    }

    pub fn select_edge(&mut self, last: bool) -> Vec<Effect> {
        let moved = if last {
            self.list.last()
        } else {
            self.list.first()
        };
        if moved {
            self.selection_changed()
        } else {
            Vec::new()
        }
    }

    // ── Timers and events ───────────────────────────────────────────────

    pub fn timer(&mut self, timer: Timer) -> Vec<Effect> {
        match timer {
            Timer::Search => self.run_search(false),
            Timer::Details => {
                let Some(p) = self.selected_package() else {
                    return Vec::new();
                };
                if self.details.contains_key(&p.id) {
                    return Vec::new();
                }
                let package = p.clone();
                self.details
                    .insert(package.id.clone(), DetailState::Loading);
                vec![Effect::FetchDetails { package }]
            }
        }
    }

    pub fn on_event(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::IndexProgress(p) => {
                if !self.refreshing {
                    self.index_state = IndexState::Loading(Some(p));
                } else {
                    self.status = Some(StatusLine {
                        kind: Kind::Info,
                        text: format::index_progress_label(&p),
                    });
                }
                Vec::new()
            }
            Event::IndexLoaded(Ok((index, origin))) => {
                let refreshed = self.refreshing;
                self.refreshing = false;
                self.index_state = IndexState::Ready {
                    packages: index.packages.len(),
                    origin,
                };
                let selected = self.selected_package().map(|p| p.id.clone());
                self.index = Some(index);
                // Old positions mean nothing in the new index.
                self.results.clear();
                self.status = refreshed.then(|| StatusLine {
                    kind: Kind::Success,
                    text: "The package index is up to date.".into(),
                });
                if refreshed {
                    self.details
                        .retain(|_, d| matches!(d, DetailState::Ready(_)));
                }
                let effects = self.run_search(true);
                if let Some(id) = selected {
                    if let Some(row) = (0..self.results.len())
                        .find(|&r| self.package(r).is_some_and(|p| p.id == id))
                    {
                        self.list.select(row);
                        return self.selection_changed();
                    }
                }
                effects
            }
            Event::IndexLoaded(Err(e)) => {
                let m = format::friendly_error(&e);
                if self.refreshing && self.index.is_some() {
                    self.refreshing = false;
                    if let Some(index) = &self.index {
                        self.index_state = IndexState::Ready {
                            packages: index.packages.len(),
                            origin: IndexOrigin::Cache,
                        };
                    }
                    self.status = Some(StatusLine {
                        kind: Kind::Error,
                        text: format!("Couldn't update the package index: {}", m.title),
                    });
                } else {
                    self.refreshing = false;
                    self.index_state = IndexState::Failed(m);
                }
                Vec::new()
            }
            Event::DetailsLoaded { id, result } => {
                let state = match result {
                    Ok(d) => DetailState::Ready(Box::new(d)),
                    Err(e) => DetailState::Failed(format::friendly_error(&e)),
                };
                self.details.insert(id, state);
                Vec::new()
            }
            Event::DownloadProgress { id, done, total } => {
                if let Some(d) = self.download.as_mut().filter(|d| d.id == id) {
                    d.done = done;
                    d.total = total;
                }
                Vec::new()
            }
            Event::DownloadFinished { id, result } => {
                let name = match self.download.take() {
                    Some(d) if d.id == id => d.name,
                    other => {
                        self.download = other;
                        id.clone()
                    }
                };
                match result {
                    Ok(done) => {
                        self.status = Some(StatusLine {
                            kind: Kind::Success,
                            text: format!(
                                "{name} downloaded and verified: {}",
                                done.path.display()
                            ),
                        });
                        self.finished.insert(id, Finished::Saved(done));
                    }
                    Err(Error::Cancelled) => {
                        self.status = Some(StatusLine {
                            kind: Kind::Info,
                            text: format!("Download of {name} cancelled."),
                        });
                        self.finished.remove(&id);
                    }
                    Err(e) => {
                        let m = format::friendly_error(&e);
                        self.status = Some(StatusLine {
                            kind: Kind::Error,
                            text: format!("{name}: {}", m.title),
                        });
                        self.finished.insert(id, Finished::Failed(m));
                    }
                }
                Vec::new()
            }
        }
    }

    /// Reports a problem from carrying out an effect (e.g. a file would
    /// not open).
    pub fn report(&mut self, kind: Kind, text: impl Into<String>) {
        self.status = Some(StatusLine {
            kind,
            text: text.into(),
        });
    }

    // ── Actions ─────────────────────────────────────────────────────────

    pub fn action(&mut self, action: &Action) -> Vec<Effect> {
        match action {
            Action::Download => self.start_download(),
            Action::Cancel => match self.download.as_mut() {
                Some(d) if !d.cancelling => {
                    d.cancelling = true;
                    vec![Effect::CancelDownload]
                }
                _ => Vec::new(),
            },
            Action::Open => match self.saved_for_selected() {
                Some(d) => vec![Effect::Open(d.path.clone())],
                None => Vec::new(),
            },
            Action::ShowInFolder => match self.saved_for_selected() {
                Some(d) => vec![Effect::ShowInFolder(d.path.clone())],
                None => Vec::new(),
            },
            Action::OpenUrl(url) => vec![Effect::OpenUrl(url.clone())],
            Action::RetryDetails => {
                let Some(p) = self.selected_package().cloned() else {
                    return Vec::new();
                };
                self.details.insert(p.id.clone(), DetailState::Loading);
                vec![Effect::FetchDetails { package: p }]
            }
            Action::RefreshIndex => {
                if self.refreshing || matches!(self.index_state, IndexState::Loading(_)) {
                    return Vec::new();
                }
                if self.index.is_some() {
                    self.refreshing = true;
                    self.status = Some(StatusLine {
                        kind: Kind::Info,
                        text: "Updating the package index...".into(),
                    });
                } else {
                    self.index_state = IndexState::Loading(None);
                }
                vec![Effect::LoadIndex { refresh: true }]
            }
        }
    }

    fn saved_for_selected(&self) -> Option<&Downloaded> {
        match self.finished.get(&self.selected_package()?.id)? {
            Finished::Saved(d) => Some(d),
            Finished::Failed(_) => None,
        }
    }

    fn start_download(&mut self) -> Vec<Effect> {
        if self.download.is_some() {
            return Vec::new();
        }
        let Some(d) = self.selected_details() else {
            return Vec::new();
        };
        let Some(inst) = d.installer() else {
            return Vec::new();
        };
        let id = self
            .selected_package()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        let effect = Effect::Download {
            id: id.clone(),
            installer: inst.clone(),
            dir: self.download_dir.clone(),
            fallback_name: d.fallback_file_name(),
        };
        self.download = Some(ActiveDownload {
            id: id.clone(),
            name: d.manifest.name.clone(),
            done: 0,
            total: None,
            started_ms: self.now_ms,
            cancelling: false,
        });
        self.finished.remove(&id);
        self.status = None;
        vec![effect]
    }

    // ── Views ───────────────────────────────────────────────────────────

    pub fn row_count(&self) -> usize {
        self.results.len()
    }

    /// "Popular apps", "12 results for “fox”".
    pub fn list_caption(&self) -> String {
        if self.index.is_none() {
            return match self.index_state {
                IndexState::Failed(_) => "No package index".into(),
                _ => "Loading...".into(),
            };
        }
        match &self.searched {
            None => "Popular apps".into(),
            Some(q) => match self.results.len() {
                0 => format!("No results for “{q}”"),
                1 => format!("1 result for “{q}”"),
                n if n >= MAX_RESULTS => format!("Top {n} results for “{q}”"),
                n => format!("{n} results for “{q}”"),
            },
        }
    }

    /// What to show in the list when it has no rows: a title and a line of
    /// explanation, and progress while the index loads.
    pub fn list_placeholder(&self) -> Option<(String, String, Option<ProgressView>)> {
        if !self.results.is_empty() {
            return None;
        }
        Some(match &self.index_state {
            IndexState::Loading(p) => (
                "Getting the catalogue ready".into(),
                p.as_ref()
                    .map(format::index_progress_label)
                    .unwrap_or_else(|| "Starting...".into()),
                Some(self.index_progress()),
            ),
            IndexState::Failed(m) => (m.title.clone(), m.detail.clone(), None),
            IndexState::Ready { .. } => match &self.searched {
                Some(q) => (
                    format!("No apps match “{q}”"),
                    "Try a shorter or different name.".into(),
                    None,
                ),
                None => (
                    "Find an app".into(),
                    "Type a name in the search box above.".into(),
                    None,
                ),
            },
        })
    }

    fn index_progress(&self) -> ProgressView {
        match &self.index_state {
            IndexState::Loading(Some(IndexProgress::Downloading { done, total })) => ProgressView {
                fraction: format::fraction(*done, *total),
                label: format::progress_label(*done, *total),
            },
            IndexState::Loading(Some(p)) => ProgressView {
                fraction: None,
                label: format::index_progress_label(p),
            },
            _ => ProgressView {
                fraction: None,
                label: String::new(),
            },
        }
    }

    pub fn row(&self, row: usize) -> Option<RowView> {
        let p = self.package(row)?;
        let publisher = match self.details.get(&p.id) {
            Some(DetailState::Ready(d)) if !d.manifest.publisher.is_empty() => {
                d.manifest.publisher.clone()
            }
            _ => p.publisher_hint().to_string(),
        };
        Some(RowView {
            name: p.name.clone(),
            publisher,
            version: p.latest_version.clone(),
            id: p.id.clone(),
            selected: self.list.selected == Some(row),
            downloaded: matches!(self.finished.get(&p.id), Some(Finished::Saved(_))),
            downloading: self.download.as_ref().is_some_and(|d| d.id == p.id),
        })
    }

    pub fn status_line(&self) -> StatusLine {
        if let Some(d) = &self.download {
            let pct = format::fraction(d.done, d.total)
                .map(|f| format!(" {}%", (f * 100.0) as u32))
                .unwrap_or_default();
            return StatusLine {
                kind: Kind::Info,
                text: format!("Downloading {}...{pct}", d.name),
            };
        }
        if let Some(s) = &self.status {
            return s.clone();
        }
        match &self.index_state {
            IndexState::Loading(p) => StatusLine {
                kind: Kind::Info,
                text: p
                    .as_ref()
                    .map(format::index_progress_label)
                    .unwrap_or_else(|| "Loading the package index...".into()),
            },
            IndexState::Failed(m) => StatusLine {
                kind: Kind::Error,
                text: m.title.clone(),
            },
            IndexState::Ready { packages, origin } => StatusLine {
                kind: Kind::Info,
                text: format!(
                    "{} apps in the winget repository{}",
                    group_thousands(*packages),
                    match origin {
                        IndexOrigin::Downloaded => " · index just downloaded",
                        IndexOrigin::Cache => "",
                    }
                ),
            },
        }
    }

    pub fn details_view(&self) -> DetailsView {
        let Some(p) = self.selected_package() else {
            return match &self.index_state {
                IndexState::Loading(_) => DetailsView::Placeholder {
                    title: "Welcome to App Center".into(),
                    detail: "Downloading the list of apps from the winget repository. This \
                             happens once a day and takes a moment."
                        .into(),
                    progress: Some(self.index_progress()),
                    button: None,
                },
                IndexState::Failed(m) => DetailsView::Placeholder {
                    title: m.title.clone(),
                    detail: format!("The list of apps could not be loaded. {}", m.detail),
                    progress: None,
                    button: Some(ButtonView {
                        label: "Try again".into(),
                        action: Action::RefreshIndex,
                        style: ButtonStyle::Primary,
                        enabled: true,
                    }),
                },
                IndexState::Ready { .. } => DetailsView::Placeholder {
                    title: "Select an app".into(),
                    detail: "Choose an app from the list to see what it is and download it.".into(),
                    progress: None,
                    button: None,
                },
            };
        };
        let mut view = PackageView {
            name: p.name.clone(),
            subtitle: join_nonempty(&[p.publisher_hint(), &p.latest_version]),
            summary: None,
            loading: false,
            buttons: Vec::new(),
            progress: None,
            notice: None,
            description: None,
            facts: Vec::new(),
        };
        match self.details.get(&p.id) {
            None | Some(DetailState::Loading) => {
                view.loading = true;
                view.facts.push(fact("Id", &p.id));
            }
            Some(DetailState::Failed(m)) => {
                view.notice = Some(Notice {
                    kind: Kind::Error,
                    title: format!("Couldn't load details: {}", m.title),
                    detail: m.detail.clone(),
                });
                view.buttons.push(ButtonView {
                    label: "Try again".into(),
                    action: Action::RetryDetails,
                    style: ButtonStyle::Primary,
                    enabled: true,
                });
                view.facts.push(fact("Id", &p.id));
            }
            Some(DetailState::Ready(d)) => self.fill_details(&mut view, p, d),
        }
        DetailsView::Package(Box::new(view))
    }

    fn fill_details(&self, view: &mut PackageView, p: &Package, d: &Details) {
        let m = &d.manifest;
        if !m.name.is_empty() {
            view.name = m.name.clone();
        }
        let publisher = if m.publisher.is_empty() {
            p.publisher_hint()
        } else {
            &m.publisher
        };
        view.subtitle = join_nonempty(&[publisher, &m.version]);
        view.summary = m.short_description.clone().filter(|s| !s.trim().is_empty());
        view.description = m
            .description
            .clone()
            .filter(|s| !s.trim().is_empty() && Some(s) != view.summary.as_ref());

        // Buttons, progress and notices.
        let ours = self.download.as_ref().filter(|a| a.id == p.id);
        let other = self.download.as_ref().filter(|a| a.id != p.id);
        let button = |label: &str, action: Action, style: ButtonStyle, enabled: bool| ButtonView {
            label: label.into(),
            action,
            style,
            enabled,
        };
        if let Some(a) = ours {
            view.buttons.push(button(
                if a.cancelling {
                    "Cancelling..."
                } else {
                    "Cancel"
                },
                Action::Cancel,
                ButtonStyle::Secondary,
                !a.cancelling,
            ));
            let elapsed = self.now_ms.saturating_sub(a.started_ms);
            let mut label = if a.done == 0 && a.total.is_none() {
                "Connecting...".to_string()
            } else {
                format::progress_label(a.done, a.total)
            };
            for extra in [
                format::speed_label(a.done, elapsed),
                format::eta_label(a.done, a.total, elapsed),
            ]
            .into_iter()
            .flatten()
            {
                label.push_str(" · ");
                label.push_str(&extra);
            }
            view.progress = Some(ProgressView {
                fraction: format::fraction(a.done, a.total),
                label,
            });
        } else {
            match (&d.choice, self.finished.get(&p.id)) {
                (Err(no), _) => {
                    let msg = format::no_choice_message(no);
                    view.notice = Some(Notice {
                        kind: Kind::Warning,
                        title: msg.title,
                        detail: msg.detail,
                    });
                }
                (Ok(_), Some(Finished::Saved(done))) => {
                    let file = done
                        .path
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    view.buttons.push(button(
                        format::open_label(&file),
                        Action::Open,
                        ButtonStyle::Primary,
                        true,
                    ));
                    view.buttons.push(button(
                        "Show in folder",
                        Action::ShowInFolder,
                        ButtonStyle::Secondary,
                        true,
                    ));
                    view.buttons.push(button(
                        "Download again",
                        Action::Download,
                        ButtonStyle::Secondary,
                        other.is_none(),
                    ));
                    view.notice = Some(Notice {
                        kind: Kind::Success,
                        title: "Downloaded and verified".into(),
                        detail: format!(
                            "Saved {} ({}) to {}. Its SHA-256 matches the repository.",
                            file,
                            crate::cli::human_size(done.size),
                            done.path
                                .parent()
                                .map(|d| d.display().to_string())
                                .unwrap_or_default()
                        ),
                    });
                }
                (Ok(_), failed) => {
                    let again = matches!(failed, Some(Finished::Failed(_)));
                    view.buttons.push(button(
                        if again { "Try again" } else { "Download" },
                        Action::Download,
                        ButtonStyle::Primary,
                        other.is_none(),
                    ));
                    if let Some(Finished::Failed(m)) = failed {
                        view.notice = Some(Notice {
                            kind: Kind::Error,
                            title: m.title.clone(),
                            detail: m.detail.clone(),
                        });
                    } else if let Some(o) = other {
                        view.notice = Some(Notice {
                            kind: Kind::Info,
                            title: "Another download is running".into(),
                            detail: format!(
                                "{} is downloading; this one can start when it is done.",
                                o.name
                            ),
                        });
                    } else if let Some(inst) = d.installer() {
                        if select::support(inst).0 == Support::NeedsMsiexec {
                            view.notice = Some(Notice {
                                kind: Kind::Warning,
                                title: "Windows Installer package".into(),
                                detail: "Only an MSI installer is available, and OTW has no \
                                         msiexec yet, so it may not install."
                                    .into(),
                            });
                        }
                    }
                }
            }
        }

        // Facts.
        view.facts.push(fact("Version", &m.version));
        view.facts.push(fact("Publisher", publisher));
        if let Some(l) = m.license.as_deref().filter(|l| !l.is_empty()) {
            view.facts.push(fact("License", l));
        }
        for (label, url) in [
            ("Homepage", m.homepage.as_deref()),
            ("Release notes", m.release_notes_url.as_deref()),
        ] {
            if let Some(url) = url.filter(|u| u.starts_with("http")) {
                view.facts.push(Fact {
                    label: label.into(),
                    value: url.into(),
                    link: Some(Action::OpenUrl(url.into())),
                });
            }
        }
        view.facts.push(fact("Id", &m.id));
        if let (Ok(c), Some(inst)) = (&d.choice, d.installer()) {
            let file = inst.file_name(&d.fallback_file_name());
            view.facts.push(fact(
                "Installer",
                &format!("{} · {} · {}", inst.arch_label(), inst.type_label(), file),
            ));
            view.facts
                .push(fact("Why this one", &capitalize(&c.reason)));
            if let Some(s) = inst.silent_switch.as_deref().filter(|s| !s.is_empty()) {
                view.facts.push(fact("Silent install", s));
            }
        }
        if !m.tags.is_empty() {
            view.facts.push(fact("Tags", &m.tags.join(", ")));
        }
        if !d.versions.is_empty() {
            view.facts.push(fact(
                &format!("Versions ({})", d.versions.len()),
                &format::versions_summary(&d.versions, 8),
            ));
        }
        view.facts
            .push(fact("Saves to", &self.download_dir.display().to_string()));
    }
}

fn fact(label: &str, value: &str) -> Fact {
    Fact {
        label: label.into(),
        value: value.into(),
        link: None,
    }
}

fn join_nonempty(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ")
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// 10422 → "10,422"
pub fn group_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{parse_manifest, parse_version_data};
    use std::path::Path;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn index() -> Index {
        Index::from_sqlite(&fixture("index.db")).unwrap()
    }

    fn seven_zip_details() -> Details {
        let m = parse_manifest(&String::from_utf8_lossy(&fixture("7zip.7zip-26.06.yaml"))).unwrap();
        let vd = crate::mszip::decompress_buffer(&fixture("7zip.versionData.mszyml")).unwrap();
        let versions = parse_version_data(&String::from_utf8_lossy(&vd)).unwrap();
        Details::new(m, &versions, &Preferences::default())
    }

    fn ready_app() -> App {
        let (mut app, effects) = App::new(
            PathBuf::from("C:\\My Documents\\Downloads"),
            Preferences::default(),
        );
        assert_eq!(effects, vec![Effect::LoadIndex { refresh: false }]);
        app.list.set_viewport(300);
        app.on_event(Event::IndexLoaded(Ok((index(), IndexOrigin::Cache))));
        app
    }

    fn type_query(app: &mut App, q: &str) -> Vec<Effect> {
        app.search.set(q);
        assert_eq!(
            app.query_edited(),
            vec![Effect::StartTimer(Timer::Search, SEARCH_DEBOUNCE_MS)]
        );
        app.timer(Timer::Search)
    }

    /// Selects 7-Zip and gives it its details.
    fn with_7zip(app: &mut App) {
        type_query(app, "7zip");
        assert_eq!(app.selected_package().unwrap().id, "7zip.7zip");
        let fx = app.timer(Timer::Details);
        assert!(matches!(&fx[..], [Effect::FetchDetails { package }] if package.id == "7zip.7zip"));
        app.on_event(Event::DetailsLoaded {
            id: "7zip.7zip".into(),
            result: Ok(seven_zip_details()),
        });
    }

    fn package_view(app: &App) -> PackageView {
        match app.details_view() {
            DetailsView::Package(p) => *p,
            other => panic!("not a package: {other:?}"),
        }
    }

    fn labels(v: &PackageView) -> Vec<&str> {
        v.buttons.iter().map(|b| b.label.as_str()).collect()
    }

    #[test]
    fn loading_shows_progress_everywhere() {
        let (mut app, _) = App::new(PathBuf::from("d"), Preferences::default());
        assert!(app.busy());
        assert_eq!(app.list_caption(), "Loading...");
        app.on_event(Event::IndexProgress(IndexProgress::Downloading {
            done: 1 << 20,
            total: Some(4 << 20),
        }));
        let (_, detail, progress) = app.list_placeholder().unwrap();
        assert!(detail.contains("25%"), "{detail}");
        assert_eq!(progress.unwrap().fraction, Some(0.25));
        assert!(app.status_line().text.contains("1.0 MiB of 4.0 MiB"));
        match app.details_view() {
            DetailsView::Placeholder { progress, .. } => {
                assert_eq!(progress.unwrap().fraction, Some(0.25))
            }
            other => panic!("{other:?}"),
        }
        // Typing before the index is there does nothing yet...
        assert!(type_query(&mut app, "notepad").is_empty());
        // ...and is searched once it arrives.
        app.on_event(Event::IndexLoaded(Ok((index(), IndexOrigin::Downloaded))));
        assert!(app.list_caption().contains("“notepad”"));
        assert!(app.status_line().text.contains("just downloaded"));
    }

    #[test]
    fn index_failure_offers_a_retry() {
        let (mut app, _) = App::new(PathBuf::from("d"), Preferences::default());
        app.on_event(Event::IndexLoaded(Err(Error::Http("dns".into()))));
        assert_eq!(app.status_line().kind, Kind::Error);
        assert_eq!(app.list_caption(), "No package index");
        let (title, _, _) = app.list_placeholder().unwrap();
        assert_eq!(title, "Can't reach the internet");
        match app.details_view() {
            DetailsView::Placeholder {
                button: Some(b), ..
            } => {
                assert_eq!(b.action, Action::RefreshIndex);
                assert_eq!(
                    app.action(&b.action),
                    vec![Effect::LoadIndex { refresh: true }]
                );
                assert!(matches!(app.index_state(), IndexState::Loading(None)));
                // A second click while loading is ignored.
                assert!(app.action(&b.action).is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn featured_apps_first() {
        let app = ready_app();
        assert_eq!(app.list_caption(), "Popular apps");
        let ids: Vec<String> = (0..app.row_count())
            .map(|r| app.row(r).unwrap().id)
            .collect();
        assert_eq!(ids[0], "Mozilla.Firefox");
        assert!(ids.contains(&"7zip.7zip".to_string()));
        assert!(ids.contains(&"Notepad++.Notepad++".to_string()));
        // The first one is selected and its details are scheduled.
        assert_eq!(app.list.selected, Some(0));
        assert!(app.status_line().text.contains("611 apps"));
    }

    #[test]
    fn searching_debounces_and_lists_results() {
        let mut app = ready_app();
        let fx = type_query(&mut app, "notepad");
        assert_eq!(
            fx,
            vec![Effect::StartTimer(Timer::Details, DETAILS_DEBOUNCE_MS)]
        );
        assert!(app.row_count() >= 4);
        assert!(app.list_caption().ends_with("results for “notepad”"));
        let row = app.row(0).unwrap();
        assert!(row.selected);
        assert!(row.name.to_lowercase().contains("notepad"));
        // The same text again does not search again.
        assert!(app.timer(Timer::Search).is_empty());
        // Enter does.
        assert!(!app.submit().is_empty() || app.row_count() > 0);
    }

    #[test]
    fn no_results_and_escape() {
        let mut app = ready_app();
        assert!(type_query(&mut app, "qqqqzzzz").is_empty());
        assert_eq!(app.row_count(), 0);
        let (title, _, _) = app.list_placeholder().unwrap();
        assert!(title.contains("qqqqzzzz"));
        assert!(matches!(
            app.details_view(),
            DetailsView::Placeholder { .. }
        ));
        app.clear_search();
        assert!(app.search.is_empty());
        assert_eq!(app.list_caption(), "Popular apps");
        assert!(app.clear_search().is_empty());
    }

    #[test]
    fn selection_fetches_details_once() {
        let mut app = ready_app();
        type_query(&mut app, "notepad");
        let fx = app.move_selection(1);
        assert_eq!(
            fx,
            vec![Effect::StartTimer(Timer::Details, DETAILS_DEBOUNCE_MS)]
        );
        let second = app.selected_package().unwrap().id.clone();
        let fx = app.timer(Timer::Details);
        assert!(matches!(&fx[..], [Effect::FetchDetails { package }] if package.id == second));
        assert!(package_view(&app).loading);
        assert!(app.busy());
        // Firing again while loading does nothing.
        assert!(app.timer(Timer::Details).is_empty());
        // Going back and forth reuses what was fetched or is in flight.
        app.move_selection(-1);
        assert_eq!(app.list.selected, Some(0));
        assert!(app.move_selection(1).is_empty());
        assert!(app.select_row(1).is_empty());
        assert!(app.select_row(999).is_empty());
        assert!(!app.select_edge(true).is_empty());
        assert_eq!(app.list.selected, Some(app.row_count() - 1));
        assert!(!app.page_selection(-1).is_empty());
    }

    #[test]
    fn details_view_has_everything() {
        let mut app = ready_app();
        with_7zip(&mut app);
        let v = package_view(&app);
        assert_eq!(v.name, "7-Zip");
        assert!(v.subtitle.contains("26.06"), "{}", v.subtitle);
        assert!(!v.loading);
        assert_eq!(labels(&v), vec!["Download"]);
        assert!(v.buttons[0].enabled);
        assert!(v.notice.is_none());
        let get = |l: &str| {
            v.facts
                .iter()
                .find(|f| f.label == l)
                .map(|f| f.value.clone())
        };
        assert_eq!(get("Version").as_deref(), Some("26.06"));
        assert!(
            get("Installer").unwrap().starts_with("x64 · exe"),
            "{:?}",
            get("Installer")
        );
        assert!(get("Why this one")
            .unwrap()
            .starts_with("Matches the requested x64"));
        assert!(get("Saves to").unwrap().contains("Downloads"));
        let versions = v
            .facts
            .iter()
            .find(|f| f.label.starts_with("Versions ("))
            .unwrap();
        assert!(versions.value.starts_with("26.06"));
        assert!(versions.value.contains("more"));
        let home = v.facts.iter().find(|f| f.label == "Homepage").unwrap();
        assert_eq!(home.link, Some(Action::OpenUrl(home.value.clone())));
        assert_eq!(
            app.action(home.link.as_ref().unwrap()),
            vec![Effect::OpenUrl(home.value.clone())]
        );
        // The list row now shows the manifest's publisher.
        assert_eq!(app.row(0).unwrap().publisher, "Igor Pavlov");
    }

    #[test]
    fn failed_details_can_be_retried() {
        let mut app = ready_app();
        type_query(&mut app, "7zip");
        app.timer(Timer::Details);
        app.on_event(Event::DetailsLoaded {
            id: "7zip.7zip".into(),
            result: Err(Error::Http("timed out".into())),
        });
        let v = package_view(&app);
        assert_eq!(v.notice.as_ref().unwrap().kind, Kind::Error);
        assert_eq!(v.buttons[0].action, Action::RetryDetails);
        let fx = app.action(&Action::RetryDetails);
        assert!(matches!(&fx[..], [Effect::FetchDetails { .. }]));
        assert!(package_view(&app).loading);
    }

    #[test]
    fn download_flow() {
        let mut app = ready_app();
        with_7zip(&mut app);
        app.set_time(1_000);
        let fx = app.action(&Action::Download);
        let [Effect::Download {
            id,
            installer,
            dir,
            fallback_name,
        }] = &fx[..]
        else {
            panic!("{fx:?}")
        };
        assert_eq!(id, "7zip.7zip");
        assert!(installer.url.ends_with("x64.exe"));
        assert_eq!(dir, Path::new("C:\\My Documents\\Downloads"));
        assert_eq!(fallback_name, "7zip.7zip-26.06.bin");
        // Only one download at a time.
        assert!(app.action(&Action::Download).is_empty());
        assert!(app.row(0).unwrap().downloading);

        let v = package_view(&app);
        assert_eq!(labels(&v), vec!["Cancel"]);
        assert_eq!(v.progress.unwrap().label, "Connecting...");

        app.set_time(3_000);
        app.on_event(Event::DownloadProgress {
            id: "7zip.7zip".into(),
            done: 1 << 20,
            total: Some(2 << 20),
        });
        let p = package_view(&app).progress.unwrap();
        assert_eq!(p.fraction, Some(0.5));
        assert!(
            p.label.starts_with("1.0 MiB of 2.0 MiB (50%)"),
            "{}",
            p.label
        );
        assert!(p.label.contains("512.0 KiB/s"), "{}", p.label);
        assert!(p.label.contains("s left"), "{}", p.label);
        assert_eq!(app.status_line().text, "Downloading 7-Zip... 50%");

        let path = PathBuf::from("C:\\My Documents\\Downloads").join("7z2606-x64.exe");
        app.on_event(Event::DownloadFinished {
            id: "7zip.7zip".into(),
            result: Ok(Downloaded {
                path: path.clone(),
                sha256: "ab".into(),
                size: 2 << 20,
            }),
        });
        assert!(app.active_download().is_none());
        let v = package_view(&app);
        assert_eq!(
            labels(&v),
            vec!["Run installer", "Show in folder", "Download again"]
        );
        let n = v.notice.unwrap();
        assert_eq!(n.kind, Kind::Success);
        assert!(n.detail.contains("7z2606-x64.exe") && n.detail.contains("2.0 MiB"));
        assert_eq!(app.status_line().kind, Kind::Success);
        assert!(app.row(0).unwrap().downloaded);
        assert_eq!(app.action(&Action::Open), vec![Effect::Open(path.clone())]);
        assert_eq!(
            app.action(&Action::ShowInFolder),
            vec![Effect::ShowInFolder(path)]
        );
    }

    #[test]
    fn hash_mismatch_is_reported_with_a_retry() {
        let mut app = ready_app();
        with_7zip(&mut app);
        app.action(&Action::Download);
        app.on_event(Event::DownloadFinished {
            id: "7zip.7zip".into(),
            result: Err(Error::HashMismatch {
                what: "x".into(),
                expected: "a".into(),
                actual: "b".into(),
            }),
        });
        let v = package_view(&app);
        assert_eq!(labels(&v), vec!["Try again"]);
        let n = v.notice.unwrap();
        assert_eq!(n.kind, Kind::Error);
        assert!(n.title.contains("integrity"));
        assert_eq!(app.status_line().kind, Kind::Error);
        assert!(app.action(&Action::Open).is_empty());
    }

    #[test]
    fn cancelling() {
        let mut app = ready_app();
        with_7zip(&mut app);
        app.action(&Action::Download);
        assert_eq!(app.action(&Action::Cancel), vec![Effect::CancelDownload]);
        assert!(app.action(&Action::Cancel).is_empty());
        let v = package_view(&app);
        assert_eq!(labels(&v), vec!["Cancelling..."]);
        assert!(!v.buttons[0].enabled);
        app.on_event(Event::DownloadFinished {
            id: "7zip.7zip".into(),
            result: Err(Error::Cancelled),
        });
        assert_eq!(labels(&package_view(&app)), vec!["Download"]);
        assert!(app.status_line().text.contains("cancelled"));
    }

    #[test]
    fn other_packages_wait_for_the_running_download() {
        let mut app = ready_app();
        with_7zip(&mut app);
        app.action(&Action::Download);
        type_query(&mut app, "Notepad++");
        let id = app.selected_package().unwrap().id.clone();
        app.timer(Timer::Details);
        let mut d = seven_zip_details();
        d.manifest.id = id.clone();
        d.manifest.name = "Notepad++".into();
        app.on_event(Event::DetailsLoaded {
            id: id.clone(),
            result: Ok(d),
        });
        let v = package_view(&app);
        assert_eq!(labels(&v), vec!["Download"]);
        assert!(!v.buttons[0].enabled);
        assert!(v.notice.unwrap().detail.contains("7-Zip"));
        // Progress of the other download is still tracked.
        app.on_event(Event::DownloadProgress {
            id: "7zip.7zip".into(),
            done: 5,
            total: Some(10),
        });
        assert_eq!(app.active_download().unwrap().done, 5);
        // An unrelated finish event leaves the running download alone.
        app.on_event(Event::DownloadFinished {
            id: "nope".into(),
            result: Err(Error::Cancelled),
        });
        assert!(app.active_download().is_some());
    }

    #[test]
    fn packaged_only_apps_explain_why() {
        let mut app = ready_app();
        type_query(&mut app, "Microsoft.WindowsTerminal");
        let id = app.selected_package().unwrap().id.clone();
        assert_eq!(id, "Microsoft.WindowsTerminal");
        app.timer(Timer::Details);
        let m = parse_manifest(&String::from_utf8_lossy(&fixture(
            "windowsterminal-1.25.yaml",
        )))
        .unwrap();
        let mut d = Details::new(m, &[], &Preferences::default());
        // Make sure it is the msix-only case, whatever the fixture holds.
        d.manifest.installers.retain(|i| i.installer_type == "msix");
        d = Details::new(d.manifest, &[], &Preferences::default());
        app.on_event(Event::DetailsLoaded { id, result: Ok(d) });
        let v = package_view(&app);
        assert!(v.buttons.is_empty());
        let n = v.notice.unwrap();
        assert_eq!(n.kind, Kind::Warning);
        assert!(n.title.contains("MSIX"));
        assert!(app.action(&Action::Download).is_empty());
    }

    #[test]
    fn msi_only_apps_warn() {
        let mut app = ready_app();
        with_7zip(&mut app);
        let mut d = seven_zip_details();
        d.manifest
            .installers
            .iter_mut()
            .for_each(|i| i.installer_type = "msi".into());
        let d = Details::new(d.manifest, &[], &Preferences::default());
        app.on_event(Event::DetailsLoaded {
            id: "7zip.7zip".into(),
            result: Ok(d),
        });
        let v = package_view(&app);
        assert_eq!(labels(&v), vec!["Download"]);
        assert!(v.notice.unwrap().detail.contains("msiexec"));
    }

    #[test]
    fn refreshing_keeps_the_old_index_usable() {
        let mut app = ready_app();
        with_7zip(&mut app);
        assert_eq!(
            app.action(&Action::RefreshIndex),
            vec![Effect::LoadIndex { refresh: true }]
        );
        assert!(app.action(&Action::RefreshIndex).is_empty());
        assert!(app.refreshing());
        assert!(app.row_count() > 0);
        app.on_event(Event::IndexProgress(IndexProgress::Unpacking));
        assert!(app.status_line().text.contains("Unpacking"));
        app.on_event(Event::IndexLoaded(Err(Error::Http("offline".into()))));
        assert_eq!(app.status_line().kind, Kind::Error);
        assert!(app.row_count() > 0);

        app.action(&Action::RefreshIndex);
        app.on_event(Event::IndexLoaded(Ok((index(), IndexOrigin::Downloaded))));
        assert_eq!(app.status_line().kind, Kind::Success);
        // Still on 7-Zip, details kept.
        assert_eq!(app.selected_package().unwrap().id, "7zip.7zip");
        assert!(!package_view(&app).loading);
    }

    #[test]
    fn thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(10422), "10,422");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }
}
