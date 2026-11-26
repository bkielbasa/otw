//! Win32 API layers.
//!
//! The genuine Win95 Notepad imports 127 functions from five DLLs. Every one
//! of them gets an implementation here: the ones that matter for startup and
//! the message loop have real behaviour, the rest return safe defaults. The
//! handles and window records live in [`SharedState`] so a host GUI backend can
//! observe what the guest asked for and render it.

pub mod advpack;
pub mod bindctx;
pub mod dwrite;
pub mod fonts;
pub mod seh64;
pub mod env;
pub mod vmem;
pub mod nt;
pub mod ucrt;
pub mod tabs;
pub mod rebar;
pub mod modern;
pub mod x64;
pub mod catmgr;
pub mod cab;
pub mod clipboard;
pub mod com;
pub mod comctl32;
pub mod commctrl;
pub mod commdlg;
pub mod controls;
pub mod crt;
pub mod cxx;
pub mod gdi;
pub mod gfx;
pub mod icon;
pub mod inf;
pub mod kernel32;
pub mod kobj;
pub mod ipc;
pub mod menu;
pub mod misc;
pub mod module;
pub mod ole;
pub mod storage;
pub mod sync;
pub mod treeview;
pub mod unicode;
pub mod version;
pub mod user;
pub mod winsock;
pub mod lnk;
pub mod profile;
pub mod registry;
pub mod rpcrt4;
pub mod shell;
pub mod shellfolder;
pub mod shelllink;
pub mod xp;
pub mod xp_user;

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
};

use crate::{
    kernel::{
        image::Image,
        pe::{ImportName, PeImage},
        syscall::{ImportPolicy, SystemCalls},
    },
    win32::winsock::WinSockState,
};

pub use crate::kernel::memory::{
    read_cstr, write_cstr, ApiOutcome, GuestMemory, Registers, Resume, ThreadRun, HEAP_BASE,
    HEAP_SIZE,
};

/// Starts the programs a program creates (`CreateProcess`). The layer that
/// runs programs provides it.
pub trait Launcher {
    /// Start `path` (on drive C:) with `command_line` in `directory`.
    /// Returns its process ID.
    fn start(&self, parent: &ApiContext, path: &[u8], command_line: &[u8], directory: &[u8]) -> Result<u32, String>;
    /// Start a program as `start` does, with what else `CreateProcess`
    /// asked for.
    fn start_with(&self, parent: &ApiContext, path: &[u8], command_line: &[u8], directory: &[u8], options: LaunchOptions) -> Result<u32, String> {
        let _ = options;
        self.start(parent, path, command_line, directory)
    }
    /// The exit code of a started program, once it has ended.
    fn exit_code(&self, pid: u32) -> Option<u32>;
}

/// What else `CreateProcess` asks for.
#[derive(Clone, Debug, Default)]
pub struct LaunchOptions {
    /// Its environment (`name=value` strings), instead of the defaults.
    pub environment: Option<Vec<String>>,
    /// The handles it inherits: `None` for all inheritable ones (when it
    /// inherits at all, see `inherit`).
    pub handles: Option<Vec<u32>>,
    pub inherit: bool,
    /// Created suspended (its main thread waits for `ResumeThread`).
    pub suspended: bool,
}

/// A program's [`Launcher`], if it may start others.
#[derive(Clone, Default)]
pub struct LauncherSlot(pub Option<Rc<dyn Launcher>>);

impl std::fmt::Debug for LauncherSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Launcher" } else { "None" })
    }
}

/// First-fit guest heap with coalescing free blocks. Block bookkeeping lives
/// on the host, so guest overruns cannot corrupt the allocator itself.
#[derive(Clone, Debug)]
pub struct GuestHeap {
    free: std::collections::BTreeMap<u32, u32>,
    used: HashMap<u32, u32>,
}

impl Default for GuestHeap {
    fn default() -> Self {
        let mut free = std::collections::BTreeMap::new();
        // Keep the first page unused so a null-adjacent pointer never aliases.
        free.insert(HEAP_BASE + 0x1000, HEAP_SIZE - 0x1000);
        Self {
            free,
            used: HashMap::new(),
        }
    }
}

impl GuestHeap {
    pub fn alloc(&mut self, size: u32) -> Option<u32> {
        let size = size.max(1).checked_add(15)? & !15;
        let (&start, &length) = self.free.iter().find(|(_, length)| **length >= size)?;
        self.free.remove(&start);
        if length > size {
            self.free.insert(start + size, length - size);
        }
        self.used.insert(start, size);
        Some(start)
    }

    pub fn free(&mut self, address: u32) -> bool {
        let Some(size) = self.used.remove(&address) else {
            return false;
        };
        let mut start = address;
        let mut length = size;
        if let Some((&prev, &prev_len)) = self.free.range(..start).next_back() {
            if prev + prev_len == start {
                self.free.remove(&prev);
                start = prev;
                length += prev_len;
            }
        }
        if let Some(&next_len) = self.free.get(&(start + length)) {
            self.free.remove(&(start + length));
            length += next_len;
        }
        self.free.insert(start, length);
        true
    }

    /// Take `[start, end)` out of the free space (a program image mapped
    /// inside the heap's range).
    pub fn reserve(&mut self, start: u32, end: u32) {
        let overlapping: Vec<(u32, u32)> = self
            .free
            .iter()
            .filter(|(&block, &length)| block < end && block + length > start)
            .map(|(&block, &length)| (block, length))
            .collect();
        for (block, length) in overlapping {
            self.free.remove(&block);
            if block < start {
                self.free.insert(block, start - block);
            }
            if block + length > end {
                self.free.insert(end, block + length - end);
            }
        }
    }

    /// More free space (a region the CPU has mapped).
    pub fn add_range(&mut self, start: u32, length: u32) {
        self.free.insert(start, length);
    }

    pub fn size_of(&self, address: u32) -> Option<u32> {
        self.used.get(&address).copied()
    }

    /// Find the allocation containing `address`, for GlobalHandle.
    pub fn block_containing(&self, address: u32) -> Option<u32> {
        self.used
            .iter()
            .find(|(start, size)| address >= **start && address < **start + **size)
            .map(|(start, _)| *start)
    }
}

struct NoGuestMemory;

impl GuestMemory for NoGuestMemory {
    fn read_bytes(&mut self, _va: u32, _len: usize) -> Option<Vec<u8>> {
        None
    }
    fn write_bytes(&mut self, _va: u32, _bytes: &[u8]) -> Option<()> {
        None
    }
    fn write_u32(&mut self, _va: u32, _value: u32) -> Option<()> {
        None
    }
}

/// A window created by the guest with `CreateWindowExA`.
///
/// Top-level windows store their position in screen coordinates; child
/// windows store it relative to their parent's client area. `width` and
/// `height` always describe the full window rectangle, including frame.
#[derive(Clone, Debug, Default)]
pub struct GuestWindow {
    pub hwnd: u32,
    /// The thread that created it, whose message loop gets its messages
    /// (0: any thread).
    pub thread: u32,
    pub class: String,
    pub title: String,
    pub parent: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub style: u32,
    pub wndproc: u32,
    pub text: Vec<u8>,
    pub visible: bool,
    pub ex_style: u32,
    pub owner: u32,
    /// Control id for child windows.
    pub id: u32,
    /// Menu bar for top-level windows.
    pub menu: u32,
    pub instance: u32,
    pub user_data: u32,
    /// `cbWndExtra` bytes reachable through Get/SetWindowLongA.
    pub extra: Vec<u8>,
    pub disabled: bool,
    /// Pending update region in client coordinates.
    pub update: crate::win32::gfx::Region,
    /// The update region needs WM_ERASEBKGND before WM_PAINT.
    pub erase: bool,
    /// Non-client area must be redrawn.
    pub nc_dirty: bool,
    pub props: Vec<(String, u32)>,
    /// Horizontal and vertical scroll bar state: (min, max, pos, page).
    pub scroll: [(i32, i32, i32, u32); 2],
    pub font: u32,
    pub maximized: bool,
    pub minimized: bool,
    /// Built-in control state (EDIT text lives in `text`).
    pub control: ControlState,
    /// Dialog procedure for dialog windows.
    pub dlg_proc: u32,
    pub dlg_result: Option<u32>,
    pub dlg_lparam: u32,
}

/// State kept by the built-in control classes.
#[derive(Clone, Debug, Default)]
pub struct ControlState {
    /// Button check state (BST_*), static icon handle, etc.
    pub check: u32,
    pub pressed: bool,
    /// EDIT: caret byte offset and selection anchor.
    pub caret: usize,
    pub anchor: usize,
    pub scroll_line: i32,
    /// LISTBOX / COMBOBOX items with their item data.
    pub items: Vec<(Vec<u8>, u32)>,
    pub selected: i32,
    pub top: i32,
    pub limit: u32,
    pub modified: bool,
    pub dropped: bool,
}

/// A classic Win32 `MSG` structure as the guest sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestMessage {
    pub hwnd: u32,
    pub message: u32,
    pub wparam: u32,
    pub lparam: u32,
}

/// An icon in the taskbar notification area (`NOTIFYICONDATA`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayIcon {
    pub hwnd: u32,
    pub id: u32,
    /// Message posted to `hwnd` for mouse events on the icon.
    pub callback: u32,
    pub icon: u32,
    pub tip: String,
}

impl GuiState {
    /// The small image of an icon handle.
    pub fn small_icon(&self, handle: u32) -> Option<&icon::IconImage> {
        self.icons.get(&handle).map(|icon| &icon.small)
    }

    /// Report a mouse event (`WM_LBUTTONDOWN`, ...) on tray icon `index` to
    /// the program, as its callback message.
    pub fn tray_event(&mut self, index: usize, mouse_message: u32) {
        if let Some(icon) = self.tray.get(index).cloned() {
            if icon.callback != 0 {
                self.enqueue(GuestMessage {
                    hwnd: icon.hwnd,
                    message: icon.callback,
                    wparam: icon.id,
                    lparam: mouse_message,
                });
            }
        }
    }
}

/// A registered window class.
#[derive(Clone, Debug, Default)]
pub struct GuestClass {
    pub name: String,
    pub wndproc: u32,
    pub style: u32,
    pub cls_extra: u32,
    pub wnd_extra: u32,
    pub instance: u32,
    pub icon: u32,
    pub cursor: u32,
    pub background: u32,
    pub menu_name: u32,
    pub atom: u32,
}

/// One raw host input event, in desktop (screen) coordinates. USER converts
/// these into window messages when the guest reads its queue, the same way
/// the Windows raw input thread feeds application queues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawInput {
    Move {
        x: i32,
        y: i32,
    },
    /// `button`: 0 left, 1 right, 2 middle.
    Button {
        x: i32,
        y: i32,
        button: u8,
        down: bool,
    },
    /// Virtual key transition plus an optional translated character.
    Key {
        vk: u32,
        down: bool,
        ch: Option<u32>,
    },
}

/// A `SetWindowsHookExA` registration.
#[derive(Clone, Copy, Debug)]
pub struct GuestHook {
    pub handle: u32,
    pub kind: i32,
    pub proc_va: u32,
}

/// A `SetTimer` registration.
#[derive(Clone, Copy, Debug)]
pub struct GuestTimer {
    pub hwnd: u32,
    /// The thread that set it (0: any).
    pub thread: u32,
    pub id: u32,
    pub interval: u32,
    pub proc_va: u32,
    pub due: u64,
}

/// One menu item.
#[derive(Clone, Debug, Default)]
pub struct MenuItem {
    pub flags: u32,
    pub id: u32,
    pub text: Vec<u8>,
    pub submenu: u32,
}

/// A menu bar or popup menu.
#[derive(Clone, Debug, Default)]
pub struct GuestMenu {
    pub items: Vec<MenuItem>,
    pub popup: bool,
}

/// Persistent addresses behind the MSVC runtime's pointer-returning globals.
#[derive(Clone, Copy, Debug, Default)]
pub struct CrtGlobals {
    pub initialized: bool,
    pub argc: u32,
    pub argv_slot: u32,
    pub argv: u32,
    pub command_line: u32,
    pub commode: u32,
    pub fmode: u32,
    pub rand_state: u32,
}

/// Observable state of the emulated window system.
#[derive(Clone, Debug)]
pub struct GuiState {
    pub windows: Vec<GuestWindow>,
    pub classes: Vec<GuestClass>,
    pub queue: VecDeque<GuestMessage>,
    pub next_hwnd: u32,
    pub quit: bool,
    pub quit_code: u32,
    /// The thread that posted the quit (0: any).
    pub quit_thread: u32,
    /// Set whenever something changed that the host renderer should redraw.
    pub dirty: bool,
    pub main_hwnd: u32,
    pub edit_hwnd: u32,
    /// Top-level window currently activated by the host window manager.
    pub active_hwnd: u32,
    /// Window currently receiving keyboard input (usually an EDIT child).
    pub focus_hwnd: u32,
    /// Messages posted with PostMessageA that the guest has not fetched yet.
    pub posted: VecDeque<GuestMessage>,
    /// Raw host input waiting to be turned into messages.
    pub input: VecDeque<RawInput>,
    /// Last known cursor position in screen coordinates.
    pub cursor: (i32, i32),
    /// Virtual key state, bit 0x80 = down, bit 0x01 = toggled.
    pub keys: Vec<u8>,
    pub capture: u32,
    pub hooks: Vec<GuestHook>,
    pub timers: Vec<GuestTimer>,
    pub menus: std::collections::HashMap<u32, GuestMenu>,
    pub next_menu: u32,
    pub atoms: Vec<String>,
    /// Screen position and time stamp of the message last retrieved.
    pub message_pos: (i32, i32),
    pub message_time: u32,
    /// Window and point of the last button press, for double-click detection.
    pub last_click: (u32, u32, i32, i32, u64),
    /// Desktop size reported to the guest (the host window's size).
    pub screen: (i32, i32),
    /// Set while a modal loop (menu tracking, window move) owns input.
    pub modal: u32,
    /// Open popup menus while a menu loop runs: (menu, popup hwnd, hot item).
    pub open_menus: Vec<(u32, u32, i32)>,
    /// Menu bar item highlighted during menu tracking.
    pub menu_bar_hot: Option<(u32, i32)>,
    pub caret: (u32, i32, i32, i32, i32, bool),
    /// WM_CHAR messages waiting for TranslateMessage of their key-down.
    pub pending_chars: VecDeque<GuestMessage>,
    /// Activation requested by the host shell (taskbar click).
    pub pending_activate: Option<u32>,
    /// Another program became the foreground: deactivate this one.
    pub pending_deactivate: bool,
    /// Images behind icon handles (`LoadIconA`).
    pub icons: HashMap<u32, icon::Icon>,
    /// This program's icons in the taskbar notification area
    /// (`Shell_NotifyIconA`), in the order they were added.
    pub tray: Vec<TrayIcon>,
    /// Window and dialog procedures that expect UTF-16 messages.
    pub unicode_procs: std::collections::HashSet<u32>,
    /// Open property sheets by frame window (0 while one is being created).
    pub property_sheets: HashMap<u32, comctl32::Sheet>,
    /// HPROPSHEETPAGEs made by CreatePropertySheetPageW.
    pub unicode_pages: std::collections::HashSet<u32>,
    /// State of SysListView32 controls.
    pub list_views: HashMap<u32, comctl32::ListView>,
    pub tree_views: HashMap<u32, treeview::TreeView>,
    pub rebars: HashMap<u32, rebar::Rebar>,
    pub tab_controls: HashMap<u32, tabs::Tabs>,
    /// Toolbars, status bars and subclassed windows.
    pub toolbars: HashMap<u32, commctrl::Toolbar>,
    pub status_bars: HashMap<u32, commctrl::StatusBar>,
    pub subclasses: HashMap<u32, commctrl::Subclasses>,
    /// BeginDeferWindowPos batches.
    pub deferred: std::collections::HashMap<u32, Vec<[u32; 7]>>,
    /// Accelerator tables: (fVirt flags, key, command).
    pub accelerators: std::collections::HashMap<u32, Vec<(u32, u32, u32)>>,
    pub idle_exits: u32,
    /// Scripted headless input (OTW_INPUT), parsed on first use.
    pub script: Option<VecDeque<String>>,
    pub script_wait: u32,
    /// The end of a scripted wait in seconds (`t n`): the script goes on
    /// then even while the program is busy.
    pub script_until: Option<std::time::Instant>,
    /// Alt (or F10) was pressed with no other key since: its release
    /// activates the menu bar.
    pub alt_alone: bool,
}

impl Default for GuiState {
    fn default() -> Self {
        Self {
            windows: Vec::new(),
            classes: Vec::new(),
            queue: VecDeque::new(),
            next_hwnd: 0x1000,
            quit: false,
            quit_thread: 0,
            quit_code: 0,
            dirty: true,
            main_hwnd: 0,
            edit_hwnd: 0,
            active_hwnd: 0,
            focus_hwnd: 0,
            posted: VecDeque::new(),
            input: VecDeque::new(),
            cursor: (0, 0),
            keys: vec![0; 256],
            capture: 0,
            hooks: Vec::new(),
            timers: Vec::new(),
            menus: std::collections::HashMap::new(),
            next_menu: 0x0000_6000,
            atoms: Vec::new(),
            message_pos: (0, 0),
            message_time: 0,
            last_click: (0, 0, 0, 0, 0),
            screen: (1024, 700),
            modal: 0,
            open_menus: Vec::new(),
            menu_bar_hot: None,
            caret: (0, 0, 0, 0, 0, false),
            pending_chars: VecDeque::new(),
            pending_activate: None,
            pending_deactivate: false,
            icons: HashMap::new(),
            tray: Vec::new(),
            unicode_procs: std::collections::HashSet::new(),
            property_sheets: HashMap::new(),
            unicode_pages: std::collections::HashSet::new(),
            list_views: HashMap::new(),
            tree_views: HashMap::new(),
            rebars: HashMap::new(),
            tab_controls: HashMap::new(),
            toolbars: HashMap::new(),
            status_bars: HashMap::new(),
            subclasses: HashMap::new(),
            deferred: std::collections::HashMap::new(),
            accelerators: std::collections::HashMap::new(),
            idle_exits: 0,
            script: None,
            script_wait: 0,
            script_until: None,
            alt_alone: false,
        }
    }
}

impl GuiState {
    pub fn window(&self, hwnd: u32) -> Option<&GuestWindow> {
        self.windows.iter().find(|window| window.hwnd == hwnd)
    }

    pub fn window_mut(&mut self, hwnd: u32) -> Option<&mut GuestWindow> {
        self.windows.iter_mut().find(|window| window.hwnd == hwnd)
    }

    pub fn wndproc_for(&self, hwnd: u32) -> Option<u32> {
        let window = self.window(hwnd)?;
        if window.wndproc != 0 {
            return Some(window.wndproc);
        }
        self.classes
            .iter()
            .find(|class| class.name.eq_ignore_ascii_case(&window.class))
            .map(|class| class.wndproc)
            .filter(|wndproc| *wndproc != 0)
    }

    pub fn is_edit_control(&self, hwnd: u32) -> bool {
        self.window(hwnd)
            .map(|window| window.class.eq_ignore_ascii_case("edit"))
            .unwrap_or(false)
    }

    /// Walk up the parent chain to find the top-level owner of `hwnd`.
    pub fn top_level_for(&self, hwnd: u32) -> Option<u32> {
        let mut current = self.window(hwnd)?;
        for _ in 0..16 {
            if current.parent == 0 {
                return Some(current.hwnd);
            }
            current = self.window(current.parent)?;
        }
        None
    }

    /// Find the EDIT control that belongs to a top-level window.
    pub fn child_edit_for(&self, hwnd: u32) -> Option<u32> {
        self.find_edit_child(hwnd, 8)
    }

    /// Text rendered in a top-level window's client area.
    pub fn client_text_for(&self, hwnd: u32) -> Option<&[u8]> {
        let edit = self.child_edit_for(hwnd)?;
        self.window(edit).map(|window| window.text.as_slice())
    }

    fn find_edit_child(&self, parent: u32, depth: usize) -> Option<u32> {
        if depth == 0 {
            return None;
        }
        for window in &self.windows {
            if window.parent == parent && window.class.eq_ignore_ascii_case("edit") {
                return Some(window.hwnd);
            }
        }
        for window in &self.windows {
            if window.parent == parent {
                if let Some(hwnd) = self.find_edit_child(window.hwnd, depth - 1) {
                    return Some(hwnd);
                }
            }
        }
        None
    }

    pub fn first_visible_top_level(&self) -> Option<u32> {
        self.windows
            .iter()
            .find(|window| window.parent == 0 && window.visible)
            .map(|window| window.hwnd)
    }

    /// Change keyboard focus, notifying both the old and new focus windows.
    pub fn set_focus(&mut self, hwnd: u32) {
        if self.focus_hwnd == hwnd {
            return;
        }
        let previous = self.focus_hwnd;
        self.focus_hwnd = hwnd;
        if previous != 0 {
            self.enqueue(GuestMessage {
                hwnd: previous,
                message: 0x0008, // WM_KILLFOCUS
                wparam: hwnd,
                lparam: 0,
            });
        }
        if hwnd != 0 {
            self.enqueue(GuestMessage {
                hwnd,
                message: 0x0007, // WM_SETFOCUS
                wparam: previous,
                lparam: 0,
            });
        }
        self.dirty = true;
    }

    /// Activate a top-level window, update z-order, and move focus to its EDIT
    /// child when it has one.
    pub fn activate_window(&mut self, hwnd: u32) {
        let Some(top) = self.top_level_for(hwnd) else {
            return;
        };
        if self.active_hwnd == top {
            return;
        }
        let previous = self.active_hwnd;
        if previous != 0 {
            self.enqueue(GuestMessage {
                hwnd: previous,
                message: 0x0006, // WM_ACTIVATE
                wparam: 0,       // WA_INACTIVE
                lparam: 0,
            });
            if self.top_level_for(self.focus_hwnd) == Some(previous) {
                self.set_focus(0);
            }
        }
        self.active_hwnd = top;
        if let Some(index) = self.windows.iter().position(|window| window.hwnd == top) {
            let window = self.windows.remove(index);
            self.windows.push(window);
        }
        self.enqueue(GuestMessage {
            hwnd: top,
            message: 0x0006, // WM_ACTIVATE
            wparam: 1,       // WA_ACTIVE
            lparam: 0,
        });
        let focus = self.child_edit_for(top).unwrap_or(top);
        self.set_focus(focus);
        self.dirty = true;
    }

    pub fn enqueue(&mut self, message: GuestMessage) {
        self.queue.push_back(message);
    }
}

/// State shared between API handlers, the CPU backend and the host GUI.
#[derive(Debug)]
pub struct SharedState {
    pub gui: GuiState,
    /// A 64-bit process (see [`x64`]).
    pub x64: bool,
    pub heap_next: u32,
    pub next_handle: u32,
    pub registered_messages: u32,
    pub last_error: u32,
    /// Guest-visible WinSock 1.1 state backed by host sockets.
    pub winsock: WinSockState,
    /// Win95 offers 64 per-process thread-local storage slots. `None` means
    /// the slot has not been allocated by `TlsAlloc`.
    pub tls_slots: Vec<Option<u32>>,
    pub crt: CrtGlobals,
    pub heap: GuestHeap,
    pub gdi: crate::win32::gdi::GdiState,
    /// Host files opened by CreateFileA/_lopen/fopen, keyed by guest handle.
    pub files: HashMap<u32, std::fs::File>,
    /// Process start, the origin of GetTickCount.
    pub started: std::time::Instant,
    pub crt_tables: crate::win32::crt::CrtTables,
    pub ucrt: ucrt::State,
    pub nt: nt::State,
    pub seh: seh64::State,
    pub dwrite: dwrite::State,
    pub vmem: vmem::State,
    pub env: env::State,
    /// A floating-point result the CPU backend must load into ST(0).
    pub fpu_result: Option<f64>,
    /// Global atoms (GlobalAddAtomA).
    pub global_atoms: Vec<String>,
    /// Paths of open file handles, for GetFileTime and friends.
    pub file_paths: HashMap<u32, Vec<u8>>,
    /// Sizes of resources located by FindResourceA.
    pub resource_sizes: HashMap<u32, u32>,
    /// Open FindFirstFileA searches: (directory, pattern, next index).
    pub finds: HashMap<u32, (Vec<u8>, Vec<u8>, usize)>,
    /// Guest address of the IMalloc object returned by CoGetMalloc.
    pub imalloc: u32,
    /// Guest address of the running object table object.
    pub rot: u32,
    /// In-memory OLE structured storage objects.
    pub storage: crate::win32::storage::StorageState,
    /// C++ exceptions whose catch block is running: (object, ThrowInfo),
    /// innermost last. `throw;` rethrows the top entry.
    pub eh_current: Vec<(u32, u32)>,
    /// The IDataObject returned by OleGetClipboard in this process.
    pub clipboard_object: u32,
    /// System clipboard contents (kept across process launches).
    pub clipboard: crate::win32::clipboard::Clipboard,
    /// The open ChooseColorA dialog, if any.
    pub color_dialog: Option<crate::win32::commdlg::ColorDialogState>,
    /// Functions handed out by GetProcAddress for Rust-backed DLLs, indexed
    /// by dynamic thunk slot: (dll, name).
    pub dynamic_imports: Vec<(String, String)>,
    /// `dynamic_imports` by (lower-case DLL, name).
    pub dynamic_index: HashMap<(String, String), usize>,
    /// UTF-16 strings handed out once per process (GetCommandLineW).
    pub wide_strings: HashMap<String, u32>,
    /// The program's SetUnhandledExceptionFilter function.
    pub unhandled_filter: u32,
    /// The program's own guest path (GetModuleFileName), when known.
    pub program_path: Vec<u8>,
    /// VirtualAlloc regions: page-aligned address to (heap block, size).
    pub virtual_allocs: HashMap<u32, (u32, u32)>,
    /// File mapping objects: handle to (file handle or 0, size).
    pub file_mappings: HashMap<u32, (u32, u32)>,
    /// Mapped views: address to (mapping handle, offset, size).
    pub mapped_views: HashMap<u32, (u32, u32, u32)>,
    /// The contents of mappings backed by the paging file (no file),
    /// as of the last view unmapped or synchronised.
    pub section_bytes: HashMap<u32, Vec<u8>>,
    /// ShowCursor's display count.
    pub cursor_count: i32,
    /// The large and small system image lists (SHGetFileInfo).
    pub system_image_lists: [u32; 2],
    /// Class objects registered with CoRegisterClassObject.
    pub class_objects: Vec<crate::win32::com::ClassObject>,
    /// The objects CoSetState keeps, by thread.
    pub com_state: HashMap<u32, u32>,
    /// Image lists (COMCTL32), by handle.
    pub image_lists: HashMap<u32, commctrl::ImageList>,
    /// Shell link objects (CLSID_ShellLink) being edited, by address.
    pub shell_links: HashMap<u32, shelllink::LinkObject>,
    /// Shell folder and item enumeration objects, by guest address.
    pub shell_folders: HashMap<u32, shellfolder::Folder>,
    pub shell_enumerations: HashMap<u32, shellfolder::Enumeration>,
    pub desktop_folder: u32,
    pub bind_contexts: HashMap<u32, bindctx::BindCtx>,
    pub categories: catmgr::CatState,
    /// Threads and synchronisation objects.
    pub sync: sync::SyncState,
    /// Starts programs for CreateProcess.
    pub launcher: LauncherSlot,
    /// The process id (1 for the first program; processes started to run
    /// alongside it get their own).
    pub pid: u32,
    /// Started to run alongside its parent: headless input scripts and
    /// idle exits are the first program's.
    pub alongside: bool,
    /// Pipes, sections and completion ports of this process.
    pub ipc: ipc::State,
    /// The program asked Windows to restart (ExitWindowsEx).
    pub restart_requested: bool,
    /// The current directory (a guest path such as `C:\\WINDOWS`).
    pub current_dir: Vec<u8>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            gui: GuiState::default(),
            x64: false,
            heap_next: HEAP_BASE,
            next_handle: 0x200,
            registered_messages: 0xC000,
            last_error: 0,
            winsock: WinSockState::default(),
            tls_slots: vec![None; 64],
            crt: CrtGlobals::default(),
            heap: GuestHeap::default(),
            gdi: crate::win32::gdi::GdiState::default(),
            files: HashMap::new(),
            started: std::time::Instant::now(),
            crt_tables: crate::win32::crt::CrtTables::default(),
            ucrt: ucrt::State::default(),
            nt: nt::State::default(),
            seh: seh64::State::default(),
            dwrite: dwrite::State::default(),
            vmem: vmem::State::default(),
            env: env::State::default(),
            fpu_result: None,
            global_atoms: Vec::new(),
            file_paths: HashMap::new(),
            resource_sizes: HashMap::new(),
            finds: HashMap::new(),
            imalloc: 0,
            rot: 0,
            color_dialog: None,
            clipboard: crate::win32::clipboard::Clipboard::default(),
            clipboard_object: 0,
            eh_current: Vec::new(),
            storage: crate::win32::storage::StorageState::default(),
            dynamic_imports: Vec::new(),
            dynamic_index: HashMap::new(),
            wide_strings: HashMap::new(),
            unhandled_filter: 0,
            current_dir: b"C:\\".to_vec(),
            shell_links: HashMap::new(),
            shell_folders: HashMap::new(),
            shell_enumerations: HashMap::new(),
            desktop_folder: 0,
            bind_contexts: HashMap::new(),
            categories: catmgr::CatState::default(),
            sync: sync::SyncState::default(),
            launcher: LauncherSlot::default(),
            pid: 1,
            alongside: false,
            ipc: ipc::State::default(),
            restart_requested: false,
            image_lists: HashMap::new(),
            cursor_count: 0,
            system_image_lists: [0; 2],
            class_objects: Vec::new(),
            com_state: HashMap::new(),
            program_path: Vec::new(),
            virtual_allocs: HashMap::new(),
            file_mappings: HashMap::new(),
            mapped_views: HashMap::new(),
            section_bytes: HashMap::new(),
        }
    }
}

impl SharedState {
    /// Clear all per-process state before another desktop shortcut starts.
    /// The `Rc` itself is preserved so the host GUI keeps observing it.
    pub fn reset_for_process(&mut self) {
        self.gui = GuiState::default();
        self.heap_next = HEAP_BASE;
        self.next_handle = 0x200;
        self.registered_messages = 0xC000;
        self.last_error = 0;
        self.winsock = WinSockState::default();
        self.tls_slots = vec![None; 64];
        self.crt = CrtGlobals::default();
        self.heap = GuestHeap::default();
        self.gdi = crate::win32::gdi::GdiState::default();
        self.files.clear();
        self.started = std::time::Instant::now();
        self.crt_tables = crate::win32::crt::CrtTables::default();
        self.fpu_result = None;
        self.global_atoms.clear();
        self.file_paths.clear();
        self.resource_sizes.clear();
        self.finds.clear();
        self.imalloc = 0;
        self.rot = 0;
        self.color_dialog = None;
        // Clipboard data outlives the process; its handles do not.
        self.clipboard.reset_process();
        self.clipboard_object = 0;
        self.eh_current.clear();
        self.storage = crate::win32::storage::StorageState::default();
        self.dynamic_imports.clear();
        self.dynamic_index.clear();
        self.sync = sync::SyncState::default();
    }

    /// Milliseconds since the process started.
    pub fn ticks(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub(crate) fn alloc_handle(&mut self) -> u32 {
        self.next_handle = self.next_handle.wrapping_add(1);
        if self.next_handle < 0x200 {
            self.next_handle = 0x200;
        }
        self.next_handle
    }

    /// Allocate guest heap memory. The name is historical: allocations are
    /// now freeable through [`GuestHeap::free`].
    pub(crate) fn bump_alloc(&mut self, size: u32) -> u32 {
        self.heap.alloc(size).unwrap_or(0)
    }
}

/// Export facts for a genuine PE module mapped into guest memory.
#[derive(Clone, Debug, Default)]
pub struct LoadedModule {
    pub name: String,
    pub image_base: u32,
    pub size: u32,
    pub exports_by_name: HashMap<String, u32>,
    pub exports_by_ordinal: HashMap<u16, u32>,
    /// Exports that forward to another DLL (`DLL.Name` or `DLL.#ordinal`).
    pub forwards_by_name: HashMap<String, String>,
    pub forwards_by_ordinal: HashMap<u16, String>,
    /// Where it was loaded from (`C:\\...`), for modules loaded at run time.
    pub path: Vec<u8>,
    /// Its entry point (DllMain), if any.
    pub entry: u32,
}

impl LoadedModule {
    pub fn from_image(name: String, image: &PeImage) -> Self {
        Self::at(name, image, image.image_base)
    }

    /// A module mapped at `base` (its preferred base unless relocated).
    pub fn at(name: String, image: &PeImage, base: u32) -> Self {
        let mut module = Self {
            name,
            image_base: base,
            size: image.size_of_image,
            entry: if image.entry_rva == 0 { 0 } else { base.wrapping_add(image.entry_rva) },
            ..Self::default()
        };
        let Some(exports) = &image.export_directory else {
            return module;
        };
        let ordinal_of = |index: usize| {
            exports
                .ordinal_base
                .checked_add(index as u32)
                .and_then(|value| u16::try_from(value).ok())
        };
        for (index, rva) in exports.addresses.iter().enumerate() {
            if *rva == 0 {
                continue;
            }
            let Some(ordinal) = ordinal_of(index) else {
                continue;
            };
            match exports.forwarders.get(&index) {
                Some(target) => {
                    module.forwards_by_ordinal.insert(ordinal, target.clone());
                }
                None => {
                    module.exports_by_ordinal.insert(ordinal, base.wrapping_add(*rva));
                }
            }
        }
        for (name, index) in &exports.names {
            let Some(rva) = exports.addresses.get(*index).filter(|rva| **rva != 0) else {
                continue;
            };
            match exports.forwarders.get(index) {
                Some(target) => {
                    module.forwards_by_name.insert(name.clone(), target.clone());
                }
                None => {
                    module.exports_by_name.insert(name.clone(), base.wrapping_add(*rva));
                }
            }
        }
        module
    }
}

/// Context shared by API handlers.
#[derive(Clone, Debug)]
pub struct ApiContext {
    pub image_base: u32,
    pub command_line_va: u32,
    /// Genuine modules available to `LoadLibraryA`/`GetProcAddress`: those
    /// mapped at start, then those loaded at run time.
    pub modules: Rc<RefCell<Vec<LoadedModule>>>,
    pub shared: Rc<RefCell<SharedState>>,
    /// Guest-callable addresses of Rust-backed USER32 window procedures,
    /// keyed by address (for direct dispatch) and by name.
    pub procs: Rc<HashMap<u32, String>>,
    pub builtin: Rc<HashMap<String, u32>>,
}

impl Default for ApiContext {
    fn default() -> Self {
        Self {
            image_base: 0,
            command_line_va: 0,
            modules: Rc::new(RefCell::new(Vec::new())),
            shared: Rc::new(RefCell::new(SharedState::default())),
            procs: Rc::new(HashMap::new()),
            builtin: Rc::new(HashMap::new()),
        }
    }
}

impl ApiContext {
    /// Address of a built-in window procedure such as `DefWindowProcA`.
    pub fn builtin_proc(&self, name: &str) -> u32 {
        self.builtin.get(name).copied().unwrap_or(0)
    }

    /// The Win32 side of a new process whose image the kernel loaded with
    /// [`entry_points`].
    pub fn for_image(image: &Image, shared: Rc<RefCell<SharedState>>) -> Self {
        let procs: HashMap<u32, String> = image
            .iat
            .entries
            .iter()
            .filter(|entry| entry.dll.eq_ignore_ascii_case("user32.dll"))
            .filter_map(|entry| {
                let name = entry.name.as_str()?;
                (name.starts_with("Def") || name.starts_with("OTW_"))
                    .then(|| (entry.thunk_va, name.to_owned()))
            })
            .collect();
        let mut builtin = HashMap::new();
        for (dll, name) in entry_points() {
            if dll != DYNAMIC_DLL {
                builtin.insert(name.clone(), image.entry_point(&dll, &name).unwrap_or(0));
            }
        }
        for module in &image.modules {
            let start = module.image.image_base & !0xFFF;
            let end = (module.image.image_base + module.memory.len() as u32 + 0xFFF) & !0xFFF;
            shared.borrow_mut().heap.reserve(start, end);
        }
        shared.borrow_mut().x64 = image.main().is64;
        if image.main().is64 {
            shared.borrow_mut().heap.add_range(crate::kernel::memory::HEAP64_EXTRA_BASE, crate::kernel::memory::HEAP64_EXTRA_SIZE);
        }
        builtin.insert(
            "__dynamic_base".to_owned(),
            image.entry_point(DYNAMIC_DLL, "slot0").unwrap_or(0),
        );
        Self {
            image_base: image.main().image_base,
            command_line_va: image.command_line_va,
            modules: Rc::new(RefCell::new(
                image
                    .modules
                    .iter()
                    .map(|module| LoadedModule::from_image(module.name.clone(), &module.image))
                    .collect(),
            )),
            shared,
            procs: Rc::new(procs),
            builtin: Rc::new(builtin),
        }
    }

    /// The function behind a call: `GetProcAddress` results for Rust-backed
    /// DLLs are handed out as numbered dynamic thunks.
    fn resolve<'a>(
        &self,
        dll: &'a str,
        name: &'a ImportName,
    ) -> Option<(std::borrow::Cow<'a, str>, std::borrow::Cow<'a, ImportName>)> {
        use std::borrow::Cow;
        if dll != DYNAMIC_DLL {
            return Some((Cow::Borrowed(dll), Cow::Borrowed(name)));
        }
        let slot = name.as_str()?.strip_prefix("slot")?.parse::<usize>().ok()?;
        let (dll, name) = self.shared.borrow().dynamic_imports.get(slot).cloned()?;
        let import = match name.strip_prefix('#').and_then(|n| n.parse::<u16>().ok()) {
            Some(ordinal) => ImportName::Ordinal(ordinal),
            None => ImportName::ByName(name),
        };
        Some((Cow::Owned(dll), Cow::Owned(import)))
    }
}

/// Pseudo-DLL of the thunks handed out by `GetProcAddress`.
const DYNAMIC_DLL: &str = "OTW_DYNAMIC";

/// Guest-callable entry points Win32 needs besides imports: built-in window
/// procedures, COM methods, and `GetProcAddress` slots.
pub fn entry_points() -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = BUILTIN_PROCS
        .iter()
        .map(|name| ("USER32.dll".to_owned(), (*name).to_owned()))
        .collect();
    entries.extend(
        BUILTIN_COM
            .iter()
            .chain(storage::STORAGE_METHODS)
            .chain(shelllink::SHELL_LINK_METHODS)
            .chain(shellfolder::METHODS)
            .chain(bindctx::METHODS)
            .chain(catmgr::methods())
            .map(|name| ("OLE32.dll".to_owned(), (*name).to_owned())),
    );
    entries.extend(dwrite::entry_names().into_iter().map(|name| ("DWrite.dll".to_owned(), name)));
    entries.push(("KERNEL32.dll".to_owned(), "OTW_ThreadStart".to_owned()));
    entries.extend(
        (0..kernel32::DYNAMIC_SLOTS).map(|slot| (DYNAMIC_DLL.to_owned(), format!("slot{slot}"))),
    );
    entries
}

/// Win32 is the environment subsystem of its processes.
impl SystemCalls for ApiContext {
    fn arg_count(&self, dll: &str, name: &ImportName) -> Option<usize> {
        let (dll, name) = self.resolve(dll, name)?;
        arg_count(&dll, &name)
    }

    fn call(
        &self,
        memory: &mut dyn GuestMemory,
        dll: &str,
        name: &ImportName,
        args: &crate::kernel::syscall::Args,
        caller: u32,
    ) -> Option<ApiOutcome> {
        let (dll, name) = self.resolve(dll, name)?;
        if args.x64() {
            return x64::call(self, memory, &dll, &name, args, caller);
        }
        dispatch_with_memory(self, memory, &dll, &name, args, caller)
    }

    fn arg_count64(&self, dll: &str, name: &ImportName) -> Option<usize> {
        let (dll, name) = self.resolve(dll, name)?;
        x64::arg_count(self, &dll, &name)
    }

    fn native_x64(&self, dll: &str, name: &ImportName) -> Option<&'static [u8]> {
        let (dll, name) = self.resolve(dll, name)?;
        if !is_ucrt(api_set_host(&dll).unwrap_or(&dll)) {
            return None;
        }
        ucrt::native_x64(name.as_str()?)
    }

    fn describe(&self, dll: &str, name: &ImportName) -> String {
        match self.resolve(dll, name) {
            Some((dll, name)) => format!("{dll}!{}", name.display()),
            None => format!("{dll}!{}", name.display()),
        }
    }
}

/// The size of a pointer in the process: 8 in a 64-bit one.
pub fn ptr_size(ctx: &ApiContext) -> u32 {
    if ctx.shared.borrow().x64 { 8 } else { 4 }
}

/// Point the COM interface pointer at `slot` at a new vtable of the
/// subsystem's `names` methods: pointer-sized slot and entries (8 bytes in
/// a 64-bit process).
pub fn set_vtable<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, slot: u32, names: &[&str]) {
    let size = memory.ptr_size();
    let table = ctx.shared.borrow_mut().heap.alloc(size * names.len() as u32).unwrap_or(0);
    for (index, name) in names.iter().enumerate() {
        let _ = memory.write_ptr(table + size * index as u32, u64::from(ctx.builtin_proc(name)));
    }
    let _ = memory.write_ptr(slot, u64::from(table));
}

/// Imports Win32 implements itself even when a genuine DLL is mapped.
pub struct Win32Imports;

impl ImportPolicy for Win32Imports {
    fn prefers_subsystem(&self, dll: &str, name: &ImportName) -> bool {
        prefer_rust_handler(dll, name)
    }
}

/// Rust-backed window procedures that get guest-callable thunk addresses.
pub const BUILTIN_PROCS: &[&str] = &[
    "DefWindowProcA",
    "DefDlgProcA",
    "OTW_ButtonProc",
    "OTW_EditProc",
    "OTW_RichEditProc",
    "OTW_StaticProc",
    "OTW_ListBoxProc",
    "OTW_ComboBoxProc",
    "OTW_ScrollBarProc",
    "OTW_MDIClientProc",
    "OTW_MenuProc",
    "OTW_ColorPanelProc",
    "OTW_PropSheetProc",
    "OTW_TabProc",
    "OTW_ListViewProc",
    "OTW_TreeViewProc",
    "OTW_TooltipProc",
    "OTW_RebarProc",
    "OTW_SubclassProc",
    "OTW_ToolbarProc",
    "OTW_StatusProc",
];

/// Rust-backed COM methods (stdcall, `this` first) exposed through vtables.
pub const BUILTIN_COM: &[&str] = &[
    "IMalloc_QueryInterface",
    "IMalloc_AddRef",
    "IMalloc_Release",
    "IMalloc_Alloc",
    "IMalloc_Realloc",
    "IMalloc_Free",
    "IMalloc_GetSize",
    "IMalloc_DidAlloc",
    "IMalloc_HeapMinimize",
    // IMoniker (file moniker): IUnknown, IPersist, IPersistStream, IMoniker.
    "Moniker_QueryInterface",
    "Moniker_AddRef",
    "Moniker_Release",
    "Moniker_GetClassID",
    "Moniker_IsDirty",
    "Moniker_Load",
    "Moniker_Save",
    "Moniker_GetSizeMax",
    "Moniker_BindToObject",
    "Moniker_BindToStorage",
    "Moniker_Reduce",
    "Moniker_ComposeWith",
    "Moniker_Enum",
    "Moniker_IsEqual",
    "Moniker_Hash",
    "Moniker_IsRunning",
    "Moniker_GetTimeOfLastChange",
    "Moniker_Inverse",
    "Moniker_CommonPrefixWith",
    "Moniker_RelativePathTo",
    "Moniker_GetDisplayName",
    "Moniker_ParseDisplayName",
    "Moniker_IsSystemMoniker",
    // IRunningObjectTable.
    "Rot_QueryInterface",
    "Rot_AddRef",
    "Rot_Release",
    "Rot_Register",
    "Rot_Revoke",
    "Rot_IsRunning",
    "Rot_GetObject",
    "Rot_NoteChangeTime",
    "Rot_GetTimeOfLastChange",
    "Rot_EnumRunning",
    // IDataObject over the clipboard.
    "DataObj_QueryInterface",
    "DataObj_AddRef",
    "DataObj_Release",
    "DataObj_GetData",
    "DataObj_GetDataHere",
    "DataObj_QueryGetData",
    "DataObj_GetCanonicalFormatEtc",
    "DataObj_SetData",
    "DataObj_EnumFormatEtc",
    "DataObj_DAdvise",
    "DataObj_DUnadvise",
    "DataObj_EnumDAdvise",
    // IEnumFORMATETC.
    "EnumFmt_QueryInterface",
    "EnumFmt_AddRef",
    "EnumFmt_Release",
    "EnumFmt_Next",
    "EnumFmt_Skip",
    "EnumFmt_Reset",
    "EnumFmt_Clone",
];

/// Number of stack arguments an API reads. For `wsprintfA` this is a fixed
/// upper bound; the handler pops zero words because it is cdecl.
/// The DLL behind a Windows 7+ API set name (`api-ms-win-core-*`,
/// `ext-ms-win-*`): KERNEL32 for the core, ADVAPI32 for security and the
/// registry, the universal C runtime for `api-ms-win-crt-*`.
pub fn is_ucrt(dll: &str) -> bool {
    dll.eq_ignore_ascii_case("ucrtbase.dll") || dll.eq_ignore_ascii_case("ucrtbase")
}

pub fn api_set_host(dll: &str) -> Option<&'static str> {
    let lower = dll.to_ascii_lowercase();
    // DLLs of later Windows whose functions Windows 95 kept elsewhere.
    match lower.as_str() {
        "combase.dll" | "combase" => return Some("OLE32.dll"),
        "kernelbase.dll" | "kernelbase" => return Some("KERNEL32.dll"),
        "ucrtbase.dll" | "ucrtbase" | "msvcp_win.dll" => return Some("ucrtbase.dll"),
        _ => {}
    }
    if !(lower.starts_with("api-ms-win-") || lower.starts_with("ext-ms-win-")) {
        return None;
    }
    let set = &lower[11..];
    if let Some(rest) = set.strip_prefix("downlevel-") {
        let dll = rest.split('-').next().unwrap_or("");
        return Some(match dll {
            "shell32" => "SHELL32.dll",
            "shlwapi" => "SHLWAPI.dll",
            "advapi32" => "ADVAPI32.dll",
            "user32" => "USER32.dll",
            "ole32" => "OLE32.dll",
            "version" => "VERSION.dll",
            _ => "KERNEL32.dll",
        });
    }
    Some(if set.starts_with("crt-") {
        "ucrtbase.dll"
    } else if set.starts_with("security-")
        || set.starts_with("eventing-")
        || set.starts_with("service-")
        || set.starts_with("core-registry-")
    {
        "ADVAPI32.dll"
    } else if set.starts_with("core-version-") {
        "VERSION.dll"
    } else if set.starts_with("core-com-") {
        "OLE32.dll"
    } else if set.starts_with("shcore-") || set.starts_with("shell-") {
        "SHELL32.dll"
    } else if set.starts_with("ntuser-") || set.starts_with("rtcore-ntuser") {
        "USER32.dll"
    } else if set.starts_with("gdi-") {
        "GDI32.dll"
    } else {
        "KERNEL32.dll"
    })
}

pub fn arg_count(dll: &str, name: &ImportName) -> Option<usize> {
    let dll = api_set_host(dll).unwrap_or(dll);
    if is_ucrt(dll) {
        return ucrt::arg_count(name.as_str()?);
    }
    if let ImportName::Ordinal(ordinal) = name {
        if let Some(named) = crate::win32::winsock::ordinal_name(dll, *ordinal) {
            return crate::win32::winsock::arg_count(named);
        }
        if let Some(named) = commctrl::ordinal_name(dll, *ordinal) {
            return commctrl::arg_count(named);
        }
        if let Some(named) = modern::comctl32_ordinal(*ordinal).filter(|_| dll.eq_ignore_ascii_case("comctl32.dll")) {
            return modern::arg_count(named);
        }
        if let Some(named) = shell::ordinal_name(dll, *ordinal) {
            return shell::arg_count(named);
        }
        // Windows 95 KERNEL32 ordinal 18 is the private, ordinal-only
        // GetProcessDword(process, index) export.
        if dll.eq_ignore_ascii_case("kernel32.dll") && *ordinal == 18 {
            return Some(2);
        }
        // Ordinals 1-9: VxDCall0..VxDCall8 (service and 0-8 arguments).
        if dll.eq_ignore_ascii_case("kernel32.dll") && (1..=9).contains(ordinal) {
            return Some(*ordinal as usize);
        }
        if let Some(count) = crate::win32::ole::arg_count(dll, name) {
            return Some(count);
        }
        return mfc::arg_count(dll, *ordinal);
    }
    if let Some(count) = crate::win32::ole::arg_count(dll, name) {
        return Some(count);
    }
    let name = match name {
        ImportName::ByName(name) => name.as_str(),
        ImportName::Ordinal(_) => unreachable!(),
    };
    if let Some(count) = sync::arg_count(name) {
        return Some(count);
    }
    if dll.eq_ignore_ascii_case("advapi32.dll") {
        if let Some(count) = registry::arg_count(name) {
            return Some(count);
        }
    }
    if dll.eq_ignore_ascii_case("rpcrt4.dll") {
        return rpcrt4::arg_count(name);
    }
    if dll.eq_ignore_ascii_case("advpack.dll") {
        return advpack::arg_count(name);
    }
    if dll.eq_ignore_ascii_case("version.dll") {
        if let Some(count) = version::arg_count(name) {
            return Some(count);
        }
    }
    if dll.eq_ignore_ascii_case("shell32.dll") {
        if let Some(count) = shell::arg_count(name) {
            return Some(count);
        }
    }
    if let Some(count) = comctl32::arg_count(name)
        .or_else(|| commctrl::arg_count(name))
        .or_else(|| xp_user::arg_count(name))
        .or_else(|| profile::arg_count(name))
        .or_else(|| unicode::arg_count(dll, name))
        .or_else(|| xp::arg_count(name))
        .or_else(|| misc::arg_count(name))
        .or_else(|| modern::arg_count(name))
    {
        return Some(count);
    }
    if dll.eq_ignore_ascii_case("wsock32.dll") || dll.eq_ignore_ascii_case("winsock.dll") || dll.eq_ignore_ascii_case("ws2_32.dll") {
        crate::win32::winsock::arg_count(name)
    } else if dll.eq_ignore_ascii_case("msvcrt20.dll") {
        msvcrt20::arg_count(name)
    } else if dll.eq_ignore_ascii_case("kernel32.dll") {
        kernel32_base::arg_count(name)
    } else if dll.eq_ignore_ascii_case("user32.dll") {
        crate::win32::user::arg_count(name)
    } else if dll.eq_ignore_ascii_case("gdi32.dll") {
        crate::win32::gdi::arg_count(name)
    } else if dll.eq_ignore_ascii_case("shell32.dll") {
        shell32::arg_count(name)
    } else if dll.eq_ignore_ascii_case("shlwapi.dll") {
        shlwapi::arg_count(name)
    } else if dll.eq_ignore_ascii_case("comdlg32.dll") {
        comdlg32::arg_count(name)
    } else {
        None
    }
}

/// Whether an API has an implementation, without calling it.
pub fn has_implementation(dll: &str, name: &ImportName) -> bool {
    arg_count(dll, name).is_some()
        || name.as_str().is_some_and(|text| nt::arg_count(text).is_some() || ipc::arg_count(text).is_some() || x64::x64_only(text).is_some())
}

/// DLLs that must keep Rust handlers even when a genuine copy is next to the
/// guest. Genuine Winsock files are useful as artifacts, but mapping them would
/// bypass the host-backed Rust socket layer. The same applies to the CRT: real
/// MSVCRT20 code is not ready to run in the guest yet.
pub fn is_rust_backed_dll(name: &str) -> bool {
    name.eq_ignore_ascii_case("wsock32.dll")
        || name.eq_ignore_ascii_case("winsock.dll")
        || name.eq_ignore_ascii_case("msvcrt20.dll")
}

/// APIs whose genuine Win95 implementations jump through KERNEL32's 16-bit
/// thunk generator. That path requires the real 16-bit kernel and VxD services,
/// so these APIs stay Rust-backed even when genuine KERNEL32 is mapped.
pub fn prefer_rust_handler(dll: &str, name: &ImportName) -> bool {
    if dll.eq_ignore_ascii_case("kernel32.dll") && matches!(name, ImportName::Ordinal(18)) {
        return true;
    }
    let ImportName::ByName(name) = name else {
        return false;
    };
    // Genuine ADVAPI32 registry code calls private KERNEL32 VxD services that
    // need the real Windows 95 kernel. Keep registry imports Rust-backed.
    if dll.eq_ignore_ascii_case("advapi32.dll") {
        return name.as_str().starts_with("Reg");
    }
    // Genuine USER32's resource loader enters KERNEL32's private module
    // database and 16-bit thunk generator before reaching the string table.
    // The Rust handler reads the same string blocks from the genuine module's
    // mapped image, avoiding that unsupported private path.
    if dll.eq_ignore_ascii_case("user32.dll") {
        // USER32's Win95 implementation repeatedly enters KERNEL32's private
        // module database and 16-bit thunk generator. The VM models windows,
        // classes, DCs and resources in Rust, so every implemented USER32
        // import stays Rust-backed rather than running that private path with
        // synthetic state.
        return crate::win32::user::arg_count(name).is_some();
    }
    // Genuine GDI32 validates HDCs against its private handle table. The Rust
    // GetDC handler issues synthetic handles, so GDI queries must stay Rust too.
    if dll.eq_ignore_ascii_case("gdi32.dll") {
        // The Rust handlers issue synthetic HDC/HBITMAP/HFONT/HRGN handles.
        // Genuine Win95 GDI32 validates every handle against its private table,
        // so running genuine GDI code with synthetic handles is unsafe. Keep
        // every implemented GDI import Rust-backed.
        return crate::win32::gdi::arg_count(name).is_some();
    }
    if !dll.eq_ignore_ascii_case("kernel32.dll") {
        return false;
    }
    matches!(
        name.as_str(),
        "SetErrorMode"
            | "GetVersion"
            | "LocalAlloc"
            | "LocalFree"
            | "LocalLock"
            | "LocalUnlock"
            | "LocalReAlloc"
            | "GlobalAlloc"
            | "GlobalFree"
            | "GlobalLock"
            | "GlobalUnlock"
            // Genuine KERNEL32's DBCS-aware lstrlen wrappers enter the private
            // module-database/VxD path before doing the actual length walk.
            // The Rust handlers are observationally equivalent for 8/16-bit
            // strings used by IE4's import thunks.
            | "lstrlenA"
            | "lstrlenW"
            // Process termination must be observed by the VM, not performed
            // by genuine KERNEL32's private VMM teardown path.
            | "ExitProcess"
            | "ExitThread"
            // Genuine KERNEL32's loader expects a real Win95 module database.
            // Rust resolution over the mapped genuine modules is faithful here:
            // it returns addresses of genuine exports, not substitute code.
            | "LoadLibraryA"
            | "GetProcAddress"
            // Skipping loader-private DllMains leaves guest critical sections
            // uninitialised; genuine EnterCriticalSection raises an exception
            // that requires the real 16-bit VMM. Rust no-ops are equivalent in
            // the VM's single-threaded execution.
            | "EnterCriticalSection"
            | "LeaveCriticalSection"
            | "InitializeCriticalSection"
            | "DeleteCriticalSection"
            // Win95's directory and profile APIs validate their buffers and
            // tail-jump into the same private 16-bit thunk generator. The Rust
            // handlers return the synthetic Win95 paths and profile defaults.
            | "GetWindowsDirectoryA"
            | "GetSystemDirectoryA"
            | "GetDriveTypeA"
            | "GetVolumeInformationA"
            | "GetFileAttributesA"
            | "GetProfileStringA"
            | "GetProfileIntA"
            | "GetPrivateProfileStringA"
            | "GetPrivateProfileIntA"
            | "GetPrivateProfileStructA"
            | "WritePrivateProfileStringA"
            | "WritePrivateProfileStructA"
            | "WriteProfileStringA"
    )
}

/// Dispatch one import call using a no-op memory view.
pub fn dispatch(ctx: &ApiContext, dll: &str, name: &ImportName, args: &[u32]) -> Option<u32> {
    dispatch_with_memory(ctx, &mut NoGuestMemory, dll, name, args, 0).map(|outcome| outcome.value)
}

/// Dispatch one import call. `args` are the raw stack arguments in order and
/// `caller` is the guest return address, used to identify the importing module
/// for resource lookups with a null instance handle.
pub fn dispatch_with_memory<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    dll: &str,
    name: &ImportName,
    args: &[u32],
    caller: u32,
) -> Option<ApiOutcome> {
    let dll = api_set_host(dll).unwrap_or(dll);
    // Fault injection for testing error paths: OTW_FAIL_API=Name=value.
    if let (Ok(spec), ImportName::ByName(import)) = (std::env::var("OTW_FAIL_API"), name) {
        if let Some((target, value)) = spec.split_once('=') {
            if target == import {
                let value = value
                    .strip_prefix("0x")
                    .map(|h| u32::from_str_radix(h, 16).ok())
                    .unwrap_or_else(|| value.parse().ok());
                if let (Some(value), Some(count)) = (value, arg_count(dll, name)) {
                    return Some(ApiOutcome::new(value, count));
                }
            }
        }
    }
    if let ImportName::Ordinal(ordinal) = name {
        if let Some(named) = crate::win32::winsock::ordinal_name(dll, *ordinal) {
            return crate::win32::winsock::dispatch(ctx, memory, &ImportName::ByName(named.into()), args);
        }
        if let Some(named) = commctrl::ordinal_name(dll, *ordinal) {
            return commctrl::dispatch(ctx, memory, named, args);
        }
        if let Some(named) = modern::comctl32_ordinal(*ordinal).filter(|_| dll.eq_ignore_ascii_case("comctl32.dll")) {
            return modern::dispatch(ctx, memory, named, args);
        }
        if let Some(named) = shell::ordinal_name(dll, *ordinal) {
            return shell::dispatch(ctx, memory, named, args);
        }
        if dll.eq_ignore_ascii_case("kernel32.dll") && *ordinal == 18 {
            return args
                .get(1)
                .map(|index| ApiOutcome::new(kernel32_base::get_process_dword(*index), 2));
        }
        if dll.eq_ignore_ascii_case("kernel32.dll") && (1..=9).contains(ordinal) {
            let value = misc::vxd_call(ctx, memory, args);
            return Some(ApiOutcome::new(value, *ordinal as usize));
        }
        if crate::win32::ole::arg_count(dll, name).is_some() {
            return crate::win32::ole::dispatch(ctx, memory, dll, name, args);
        }
        return mfc::dispatch(ctx, memory, dll, *ordinal, args);
    }
    if crate::win32::ole::arg_count(dll, name).is_some() {
        return crate::win32::ole::dispatch(ctx, memory, dll, name, args);
    }
    if let ImportName::ByName(text) = name {
        if sync::arg_count(text).is_some() {
            return sync::dispatch(ctx, memory, text, args);
        }
        if dll.eq_ignore_ascii_case("rpcrt4.dll") {
            return rpcrt4::dispatch(ctx, memory, text, args);
        }
        if dll.eq_ignore_ascii_case("advpack.dll") {
            return advpack::dispatch(ctx, memory, text, args);
        }
        if dll.eq_ignore_ascii_case("version.dll") {
            return version::dispatch(ctx, memory, text, args);
        }
        if dll.eq_ignore_ascii_case("shell32.dll") && shell::arg_count(text).is_some() {
            return shell::dispatch(ctx, memory, text, args);
        }
        if dll.eq_ignore_ascii_case("advapi32.dll") && registry::arg_count(text).is_some() {
            return registry::dispatch(ctx, memory, text, args);
        }
        if comctl32::arg_count(text).is_some() {
            return comctl32::dispatch(ctx, memory, text, args);
        }
        if commctrl::arg_count(text).is_some() {
            return commctrl::dispatch(ctx, memory, text, args);
        }
        if xp_user::arg_count(text).is_some() {
            return xp_user::dispatch(ctx, memory, text, args);
        }
        if profile::arg_count(text).is_some() {
            return profile::dispatch(ctx, memory, text, args);
        }
        if unicode::arg_count(dll, text).is_some() {
            return unicode::dispatch(ctx, memory, dll, text, args, caller);
        }
        if xp::arg_count(text).is_some() {
            return xp::dispatch(ctx, memory, text, args);
        }
        if misc::arg_count(text).is_some() {
            return misc::dispatch(ctx, memory, text, args);
        }
        if modern::arg_count(text).is_some() {
            return modern::dispatch(ctx, memory, text, args);
        }
    }
    if dll.eq_ignore_ascii_case("wsock32.dll") || dll.eq_ignore_ascii_case("winsock.dll") || dll.eq_ignore_ascii_case("ws2_32.dll") {
        crate::win32::winsock::dispatch(ctx, memory, name, args)
    } else if dll.eq_ignore_ascii_case("msvcrt20.dll") {
        msvcrt20::dispatch(ctx, memory, name, args)
    } else if dll.eq_ignore_ascii_case("kernel32.dll") {
        kernel32_base::dispatch(ctx, memory, name, args)
    } else if dll.eq_ignore_ascii_case("user32.dll") {
        crate::win32::user::dispatch(ctx, memory, name, args, caller)
    } else if dll.eq_ignore_ascii_case("gdi32.dll") {
        crate::win32::gdi::dispatch(ctx, memory, name, args)
    } else if dll.eq_ignore_ascii_case("shell32.dll") {
        shell32::dispatch(ctx, memory, name, args)
    } else if dll.eq_ignore_ascii_case("shlwapi.dll") {
        shlwapi::dispatch(ctx, memory, name, args)
    } else if dll.eq_ignore_ascii_case("comdlg32.dll") {
        comdlg32::dispatch(ctx, memory, name, args)
    } else {
        None
    }
}

pub(crate) fn name_of(name: &ImportName) -> Option<&str> {
    match name {
        ImportName::ByName(name) => Some(name.as_str()),
        ImportName::Ordinal(_) => None,
    }
}

/// Copy an ANSI string into a counted caller buffer, following the Win32
/// return convention: success returns the character count without the NUL,
/// while a buffer that is too small returns the required size including it.
pub(crate) fn copy_path<M: GuestMemory + ?Sized>(
    memory: &mut M,
    buffer: u32,
    size: u32,
    path: &[u8],
) -> u32 {
    let required = path.len() as u32 + 1;
    if buffer == 0 || size < required {
        return required;
    }
    let mut bytes = Vec::with_capacity(required as usize);
    bytes.extend_from_slice(path);
    bytes.push(0);
    if memory.write_bytes(buffer, &bytes).is_some() {
        path.len() as u32
    } else {
        required
    }
}

pub(crate) fn read_ansi<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, max: usize) -> Vec<u8> {
    if va == 0 {
        return Vec::new();
    }
    let data = memory.read_bytes(va, max.min(65536)).unwrap_or_default();
    match data.iter().position(|byte| *byte == 0) {
        Some(index) => data[..index].to_vec(),
        None => data,
    }
}

pub(crate) fn cstr_cmp(left: &[u8], right: &[u8]) -> i32 {
    for (a, b) in left.iter().zip(right.iter()) {
        if a != b {
            return i32::from(*a) - i32::from(*b);
        }
    }
    left.len() as i32 - right.len() as i32
}

// ── MSVCRT20 ───────────────────────────────────────────────────────────────

mod msvcrt20 {
    use super::*;

    /// All MSVC 2.x runtime exports are `cdecl`: callers pop their own stack.
    /// This count is only how many words the thunk reads for the handler.
    pub(super) fn arg_count(name: &str) -> Option<usize> {
        Some(match name {
            "rand" | "_purecall" | "?terminate@@YAXXZ" | "__p___argc" | "__p___argv"
            | "__p__acmdln" | "_adjust_fdiv" | "__p__commode" | "__p__fmode" => 0,
            "abs" | "free" | "atoi" | "_mkdir" | "_exit" | "exit" | "_onexit" | "??3@YAXPAX@Z"
            | "??2@YAPAXI@Z" => 1,
            "_controlfp" | "_mbscmp" | "_mbsicmp" | "_stat" | "_XcptFilter"
            | "_CxxThrowException" => 2,
            "_getdcwd" | "_itoa" | "_except_handler3" | "__dllonexit" | "memcmp" | "memcpy"
            | "memset" | "strncpy" | "_mbsnbcmp" => 3,
            "__CxxFrameHandler" => 4,
            "__getmainargs" | "_initterm" | "_splitpath" => 5,
            _ => return crate::win32::crt::arg_count(name),
        })
    }

    fn ensure_globals<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M) -> CrtGlobals {
        let current = ctx.shared.borrow().crt;
        if current.initialized {
            return current;
        }

        let globals = {
            let mut shared = ctx.shared.borrow_mut();
            let globals = CrtGlobals {
                initialized: true,
                argc: shared.bump_alloc(4),
                argv_slot: shared.bump_alloc(4),
                argv: shared.bump_alloc(8),
                command_line: shared.bump_alloc(4),
                commode: shared.bump_alloc(4),
                fmode: shared.bump_alloc(4),
                rand_state: 1,
            };
            shared.crt = globals;
            globals
        };

        let _ = memory.write_u32(globals.argc, 1);
        let _ = memory.write_u32(globals.argv, ctx.command_line_va);
        let _ = memory.write_u32(globals.argv.wrapping_add(4), 0);
        let _ = memory.write_u32(globals.argv_slot, globals.argv);
        let _ = memory.write_u32(globals.command_line, ctx.command_line_va);
        let _ = memory.write_u32(globals.commode, 0x4000);
        let _ = memory.write_u32(globals.fmode, 0x4000);
        globals
    }

    fn bounded_len(length: u32) -> usize {
        (length as usize).min(16 * 1024 * 1024)
    }

    fn parse_int(bytes: &[u8]) -> i32 {
        let mut index = 0;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let negative = bytes.get(index) == Some(&b'-');
        if matches!(bytes.get(index), Some(b'+') | Some(b'-')) {
            index += 1;
        }
        let mut value = 0i32;
        let mut digits = 0;
        while let Some(byte) = bytes.get(index) {
            if !byte.is_ascii_digit() {
                break;
            }
            value = value
                .saturating_mul(10)
                .saturating_add(i32::from(byte - b'0'));
            index += 1;
            digits += 1;
        }
        if digits == 0 {
            0
        } else if negative {
            -value
        } else {
            value
        }
    }

    fn split_path<M: GuestMemory + ?Sized>(
        ctx: &ApiContext,
        memory: &mut M,
        args: &[u32],
    ) -> ApiOutcome {
        let path = read_cstr(memory, args[0]);
        let colon = path.iter().position(|byte| *byte == b':');
        let drive_end = colon.map_or(0, |index| index + 1);
        let drive = if drive_end == 0 {
            &path[..0]
        } else {
            &path[..drive_end]
        };

        let remainder = &path[drive_end..];
        let separator = remainder
            .iter()
            .rposition(|byte| *byte == b'\\' || *byte == b'/');
        let (dir, name_ext) = match separator {
            Some(index) => (&remainder[..=index], &remainder[index + 1..]),
            None => (&remainder[..0], remainder),
        };
        let dot = name_ext.iter().rposition(|byte| *byte == b'.');
        let (name, extension) = match dot {
            Some(index) => (&name_ext[..index], &name_ext[index..]),
            None => (name_ext, &name_ext[..0]),
        };

        if args[1] != 0 {
            let _ = write_cstr(memory, args[1], drive);
        }
        if args[2] != 0 {
            let _ = write_cstr(memory, args[2], dir);
        }
        if args[3] != 0 {
            let _ = write_cstr(memory, args[3], name);
        }
        if args[4] != 0 {
            let _ = write_cstr(memory, args[4], extension);
        }
        let _ = ctx;
        ApiOutcome::cdecl(0)
    }

    pub(super) fn dispatch<M: GuestMemory + ?Sized>(
        ctx: &ApiContext,
        memory: &mut M,
        name: &ImportName,
        args: &[u32],
    ) -> Option<ApiOutcome> {
        let name = name_of(name)?;
        let count = arg_count(name)?;
        if args.len() < count {
            return None;
        }
        let globals = ensure_globals(ctx, memory);

        let outcome = match name {
            "__p___argc" => ApiOutcome::cdecl(globals.argc),
            "__p___argv" => ApiOutcome::cdecl(globals.argv_slot),
            "__p__acmdln" => ApiOutcome::cdecl(globals.command_line),
            "__p__commode" => ApiOutcome::cdecl(globals.commode),
            "__p__fmode" => ApiOutcome::cdecl(globals.fmode),
            "__getmainargs" => {
                let _ = memory.write_u32(args[0], 1);
                let _ = memory.write_u32(args[1], globals.argv);
                let _ = memory.write_u32(args[2], 0);
                ApiOutcome::cdecl(0)
            }
            "_initterm" => {
                // Run C/C++ static initialisers in [begin, end).
                let (begin, end) = (args[0], args[1]);
                let mut slot = begin;
                while slot < end && end - begin < 0x10000 {
                    let function = memory.read_u32(slot).unwrap_or(0);
                    if function != 0 && function != u32::MAX {
                        memory.call_guest(function, &[]);
                        if memory.stopping() {
                            break;
                        }
                    }
                    slot += 4;
                }
                ApiOutcome::cdecl(0)
            }
            "_adjust_fdiv" => ApiOutcome::cdecl(0),
            "_controlfp" => ApiOutcome::cdecl(0x8001_F01F),
            "_onexit" | "__dllonexit" => ApiOutcome::cdecl(args[0]),
            "?terminate@@YAXXZ" | "_purecall" => {
                eprintln!("[crt] {name} called; terminating");
                ApiOutcome {
                    wide: None,
                    value: 0,
                    arg_count: 0,
                    exit_code: Some(3),
                }
            }

            "exit" | "_exit" => ApiOutcome {
                wide: None,
                value: 0,
                arg_count: 0,
                exit_code: Some(args[0]),
            },

            "abs" => ApiOutcome::cdecl((args[0] as i32).unsigned_abs()),
            "rand" => {
                let value = {
                    let mut shared = ctx.shared.borrow_mut();
                    shared.crt.rand_state = shared
                        .crt
                        .rand_state
                        .wrapping_mul(2_140_131)
                        .wrapping_add(2_531_011);
                    (shared.crt.rand_state >> 16) & 0x7FFF
                };
                ApiOutcome::cdecl(value)
            }
            "atoi" => {
                let text = read_cstr(memory, args[0]);
                ApiOutcome::cdecl(parse_int(&text) as u32)
            }
            "_itoa" => {
                let radix = if args[2] == 0 { 10 } else { args[2] } as u32;
                let value = args[0] as i32;
                let negative = value < 0 && radix == 10;
                let mut magnitude = if negative {
                    -(value as i64)
                } else {
                    i64::from(value)
                } as u64;
                let mut digits = Vec::new();
                if magnitude == 0 {
                    digits.push(b'0');
                }
                while magnitude != 0 {
                    let digit = (magnitude % u64::from(radix)) as u8;
                    digits.push(if digit < 10 {
                        b'0' + digit
                    } else {
                        b'a' + digit - 10
                    });
                    magnitude /= u64::from(radix);
                }
                if negative {
                    digits.push(b'-');
                }
                digits.reverse();
                let _ = write_cstr(memory, args[1], &digits);
                ApiOutcome::cdecl(args[1])
            }

            "memcpy" => {
                let length = bounded_len(args[2]);
                if let Some(bytes) = memory.read_bytes(args[1], length) {
                    let _ = memory.write_bytes(args[0], &bytes);
                }
                ApiOutcome::cdecl(args[0])
            }
            "memset" => {
                let length = bounded_len(args[2]);
                let bytes = vec![args[1] as u8; length];
                let _ = memory.write_bytes(args[0], &bytes);
                ApiOutcome::cdecl(args[0])
            }
            "memcmp" => {
                let length = bounded_len(args[2]);
                let left = memory.read_bytes(args[0], length).unwrap_or_default();
                let right = memory.read_bytes(args[1], length).unwrap_or_default();
                let mut result = 0i32;
                for index in 0..length.max(left.len()).max(right.len()) {
                    let a = left.get(index).copied().unwrap_or(0);
                    let b = right.get(index).copied().unwrap_or(0);
                    if a != b {
                        result = i32::from(a) - i32::from(b);
                        break;
                    }
                }
                ApiOutcome::cdecl(result as u32)
            }
            "strncpy" => {
                let limit = bounded_len(args[2]);
                let source = read_cstr(memory, args[1]);
                let mut copy = source;
                copy.truncate(limit);
                copy.resize(limit, 0);
                let _ = memory.write_bytes(args[0], &copy);
                ApiOutcome::cdecl(args[0])
            }
            "_mbscmp" => {
                let left = read_cstr(memory, args[0]);
                let right = read_cstr(memory, args[1]);
                ApiOutcome::cdecl(cstr_cmp(&left, &right) as u32)
            }
            "_mbsicmp" => {
                let left = read_cstr(memory, args[0]).to_ascii_lowercase();
                let right = read_cstr(memory, args[1]).to_ascii_lowercase();
                ApiOutcome::cdecl(cstr_cmp(&left, &right) as u32)
            }

            "_mkdir" => ApiOutcome::cdecl(0),
            "_stat" => {
                ctx.shared.borrow_mut().last_error = 2;
                ApiOutcome::cdecl(u32::MAX)
            }
            "_getdcwd" => {
                let text = b"C:\\";
                let capacity = args[2] as usize;
                let copy = if text.len() + 1 <= capacity {
                    text.to_vec()
                } else {
                    Vec::new()
                };
                let _ = write_cstr(memory, args[1], &copy);
                ApiOutcome::cdecl(args[1])
            }
            "_splitpath" => split_path(ctx, memory, args),

            "??2@YAPAXI@Z" => {
                let address = ctx.shared.borrow_mut().heap.alloc(args[0]).unwrap_or(0);
                ApiOutcome::cdecl(address)
            }
            "??3@YAXPAX@Z" | "free" => {
                if args[0] != 0 {
                    ctx.shared.borrow_mut().heap.free(args[0]);
                }
                ApiOutcome::cdecl(0)
            }

            // __CxxFrameHandler receives the function's FuncInfo in EAX.
            "__CxxFrameHandler" => {
                let funcinfo = memory.registers().map(|r| r.eax).unwrap_or(0);
                ApiOutcome::cdecl(cxx::cxx_frame_handler(
                    ctx, memory, args[0], args[1], args[2], funcinfo,
                ))
            }
            "_except_handler3" => {
                ApiOutcome::cdecl(cxx::except_handler3(ctx, memory, args[0], args[1], args[2]))
            }
            "_XcptFilter" => ApiOutcome::cdecl(cxx::xcpt_filter(memory, args[0], args[1])),
            "_CxxThrowException" => cxx::throw(ctx, memory, args[0], args[1]),
            _ => return crate::win32::crt::dispatch(ctx, memory, name, args),
        };
        Some(outcome)
    }
}

// ── MFC ordinals ───────────────────────────────────────────────────────────

mod mfc {
    use super::*;

    /// One ordinal-only MFC export. MFC30.DLL has no name table, so the
    /// genuine DLL's export ordinal is the only stable identity available.
    /// `args` is the number of stdcall dwords the epilogue pops; `result` is
    /// the EAX value returned to the guest. Every entry is derived from the
    /// genuine Win95 RTM DLL and the MSPAINT.EXE call site rather than guessed.
    #[derive(Clone, Copy, Debug)]
    struct Entry {
        args: usize,
        result: u32,
    }

    const fn entry(args: usize, result: u32) -> Entry {
        Entry { args, result }
    }

    fn lookup(dll: &str, ordinal: u16) -> Option<Entry> {
        if dll.eq_ignore_ascii_case("mfc30.dll") {
            match ordinal {
                // AfxWinMain(HINSTANCE, HINSTANCE, LPSTR, int). The CRT startup
                // passes its result straight to exit(), so this value becomes
                // the process exit code. Without the real MFC framework there
                // is no app to run; return success so the trace stays honest.
                2146 => Some(entry(4, 0)),
                _ => None,
            }
        } else {
            None
        }
    }

    pub(super) fn arg_count(dll: &str, ordinal: u16) -> Option<usize> {
        lookup(dll, ordinal).map(|entry| entry.args)
    }

    pub(super) fn dispatch<M: GuestMemory + ?Sized>(
        ctx: &ApiContext,
        _memory: &mut M,
        dll: &str,
        ordinal: u16,
        args: &[u32],
    ) -> Option<ApiOutcome> {
        let entry = lookup(dll, ordinal)?;
        if args.len() < entry.args {
            return None;
        }
        let _ = (ctx, args);
        Some(ApiOutcome::new(entry.result, entry.args))
    }
}

// ── KERNEL32 ───────────────────────────────────────────────────────────────

mod kernel32_base {
    use super::*;

    /// Windows 95 RTM's private GetProcessDword selector values, recovered from
    /// the genuine KERNEL32 dispatch table at RVA 0x4132 and the callers in
    /// GDI32. The negative indexes used by GDI32 are Win16-compatible aliases.
    pub(super) fn get_process_dword(index: u32) -> u32 {
        match index as i32 {
            // GDI32 tests these for the 0x400 and 0x200000 feature groups.
            -44 | 0x400 => 0x400,
            -56 => 0x200000,
            // Common Win95 flags queried through this private export.
            0..=3 => 0,
            _ => 0,
        }
    }

    const STD_INPUT_HANDLE: u32 = (-10i32) as u32;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const STD_ERROR_HANDLE: u32 = (-12i32) as u32;
    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

    pub(super) fn arg_count(name: &str) -> Option<usize> {
        if let Some(count) = crate::win32::kernel32::arg_count(name) {
            return Some(count);
        }
        Some(match name {
            "GetCommandLineA" | "GetLastError" | "GetVersion" | "GetCurrentThreadId"
            | "TlsAlloc" => 0,
            "TlsGetValue"
            | "TlsFree"
            | "DisableThreadLibraryCalls"
            | "ExitThread"
            | "SetErrorMode"
            | "CloseHandle"
            | "LoadLibraryA"
            | "GetDriveTypeA"
            | "GetFileAttributesA" => 1,
            "TlsSetValue" | "GetProcAddress" | "WaitForSingleObject" => 2,
            "ExitProcess"
            | "GetModuleHandleA"
            | "GetStartupInfoA"
            | "GetStdHandle"
            | "LocalLock"
            | "LocalUnlock"
            | "LocalFree"
            | "GlobalLock"
            | "GlobalUnlock"
            | "GlobalFree"
            | "FindClose"
            | "_lclose"
            | "DeleteFileA"
            | "EnterCriticalSection"
            | "LeaveCriticalSection"
            | "InitializeCriticalSection"
            | "DeleteCriticalSection" => 1,
            "GetWindowsDirectoryA" | "GetSystemDirectoryA" | "LocalAlloc" | "GlobalAlloc" => 2,
            "lstrlenA" | "lstrlenW" => 1,
            "lstrcmpA" | "lstrcmpiA" | "lstrcatA" | "lstrcpyA" | "_lcreat" | "_lopen" => 2,
            "lstrcpynA" | "LocalReAlloc" | "_lwrite" | "_llseek" | "_lread" | "GetLocaleInfoA" => {
                if name == "GetLocaleInfoA" {
                    4
                } else {
                    3
                }
            }
            "CreateEventA" => 4,
            "CreateFileA" | "FindFirstFileA" => {
                if name == "CreateFileA" {
                    7
                } else {
                    2
                }
            }
            "GetVolumeInformationA" => 8,
            "GetLocalTime" => 1,
            "GetTimeFormatA" | "GetDateFormatA" | "MultiByteToWideChar" => 6,
            "WriteFile" => 5,
            _ => return None,
        })
    }

    pub(super) fn dispatch<M: GuestMemory + ?Sized>(
        ctx: &ApiContext,
        memory: &mut M,
        name: &ImportName,
        args: &[u32],
    ) -> Option<ApiOutcome> {
        let name = name_of(name)?;
        let count = arg_count(name)?;
        if args.len() < count {
            return None;
        }
        if crate::win32::kernel32::arg_count(name).is_some() {
            return crate::win32::kernel32::dispatch(ctx, memory, name, args);
        }

        let outcome = match name {
            // The guest is single-threaded and loader-private initialization is
            // synthetic. Real Win95 EnterCriticalSection raises 0xC0000005 when
            // it sees an uninitialised section (SHELL32's skipped DllMain would
            // have initialised several); no-op handlers preserve the observable
            // single-threaded behaviour without entering KERNEL32's 16-bit
            // exception-reporting thunk.
            "EnterCriticalSection"
            | "LeaveCriticalSection"
            | "InitializeCriticalSection"
            | "DeleteCriticalSection" => ApiOutcome::new(0, count),
            "GetModuleHandleA" => {
                ApiOutcome::new(if args[0] == 0 { ctx.image_base } else { 0 }, count)
            }
            "GetCommandLineA" => ApiOutcome::new(ctx.command_line_va, count),
            // Windows 95 RTM: major 4, minor 0, build 950.
            // Windows 95 4.00.950: the high bit marks Windows 95, not NT.
            "GetVersion" => {
                let v = crate::win32::unicode::os_version(ctx);
                // Windows 95 sets the top bit (not NT) and leaves the build high word.
                let value = if v.platform == 2 { v.build << 16 | v.minor << 8 | v.major } else { 0xC3B6_0004 };
                ApiOutcome::new(value, count)
            }
            "GetWindowsDirectoryA" => {
                ApiOutcome::new(copy_path(memory, args[0], args[1], b"C:\\WINDOWS"), count)
            }
            "GetSystemDirectoryA" => ApiOutcome::new(
                copy_path(memory, args[0], args[1], b"C:\\WINDOWS\\SYSTEM"),
                count,
            ),
            // The guest has one fixed hard disk. Genuine GetDriveTypeA calls a
            // protected-mode IFSMgr VxD service that the VM does not provide.
            "GetDriveTypeA" => {
                let path = read_cstr(memory, args[0]);
                // C: (and D:, when a program came from elsewhere) are fixed
                // disks; A: and B: floppies; others do not exist.
                let letter = path.first().map(u8::to_ascii_uppercase).unwrap_or(b'C');
                let kind = match letter {
                    b'A' | b'B' => 2,
                    b'C' => 3,
                    b'D' if crate::win32::crt::drive_d_root().is_some() => 3,
                    _ if !letter.is_ascii_alphabetic() => 3,
                    _ => 1, // DRIVE_NO_ROOT_DIR
                };
                ApiOutcome::new(kind, count)
            }
            // Genuine GetVolumeInformationA enters the same IFSMgr VxD path.
            // SHELL32 only uses the returned flags to classify the drive, so
            // synthetic FAT32 data is observationally sufficient here.
            "GetVolumeInformationA" => {
                if args[1] != 0 && args[2] > 0 {
                    let _ = write_cstr(memory, args[1], b"OTW");
                }
                if args[3] != 0 {
                    let _ = memory.write_u32(args[3], 0x1234_5678);
                }
                if args[4] != 0 {
                    let _ = memory.write_u32(args[4], 255);
                }
                if args[5] != 0 {
                    let _ = memory.write_u32(args[5], 0x0000_001f);
                }
                if args[6] != 0 && args[7] > 0 {
                    let _ = write_cstr(memory, args[6], b"FAT32");
                }
                ApiOutcome::new(1, count)
            }
            // There is no host-mounted guest file behind the synthetic drives,
            // so SHELL32's AutoRun.inf probe must observe a missing file.
            "GetFileAttributesA" => ApiOutcome::new(INVALID_FILE_ATTRIBUTES, count),
            "GetStartupInfoA" => {
                // STARTUPINFO, zeroed, in the process's layout.
                crate::win32::x64::write_struct(memory, &crate::win32::x64::STARTUPINFO, args[0], &[]);
                ApiOutcome::new(0, count)
            }
            "GetStdHandle" => {
                let handle = match args[0] {
                    STD_INPUT_HANDLE => (-10i32) as u32,
                    STD_OUTPUT_HANDLE => (-11i32) as u32,
                    STD_ERROR_HANDLE => (-12i32) as u32,
                    _ => 0,
                };
                ApiOutcome::new(handle, count)
            }
            "WriteFile" => write_file(memory, args, count),
            "GetLastError" => {
                let error = ctx.shared.borrow().last_error;
                ApiOutcome::new(error, count)
            }
            "TlsAlloc" => {
                let mut shared = ctx.shared.borrow_mut();
                match shared.tls_slots.iter().position(Option::is_none) {
                    Some(index) => {
                        shared.tls_slots[index] = Some(0);
                        shared.last_error = 0;
                        ApiOutcome::new(index as u32, count)
                    }
                    None => {
                        shared.last_error = 8; // ERROR_NOT_ENOUGH_MEMORY
                        ApiOutcome::new(u32::MAX, count)
                    }
                }
            }
            "TlsSetValue" => {
                let mut shared = ctx.shared.borrow_mut();
                let index = args[0] as usize;
                if let Some(slot) = shared.tls_slots.get_mut(index) {
                    *slot = Some(args[1]);
                    shared.last_error = 0;
                    ApiOutcome::new(1, count)
                } else {
                    shared.last_error = 87; // ERROR_INVALID_PARAMETER
                    ApiOutcome::new(0, count)
                }
            }
            "TlsGetValue" => {
                let mut shared = ctx.shared.borrow_mut();
                let index = args[0] as usize;
                let value = shared
                    .tls_slots
                    .get(index)
                    .and_then(|slot| *slot)
                    .unwrap_or(0);
                // A successful TlsGetValue always clears the last error.
                shared.last_error = 0;
                ApiOutcome::new(value, count)
            }
            "TlsFree" => {
                let mut shared = ctx.shared.borrow_mut();
                let index = args[0] as usize;
                if let Some(slot) = shared.tls_slots.get_mut(index) {
                    *slot = None;
                    shared.last_error = 0;
                    ApiOutcome::new(1, count)
                } else {
                    shared.last_error = 87;
                    ApiOutcome::new(0, count)
                }
            }
            // Single-threaded guest: no per-DLL thread notification is needed.
            "DisableThreadLibraryCalls" => ApiOutcome::new(1, count),
            "ExitProcess" | "ExitThread" => {
                registry::flush();
                ApiOutcome {
                    wide: None,
                    value: 0,
                    arg_count: count,
                    exit_code: Some(args[0]),
                }
            }
            "GetProcAddress" => {
                let requested = if args[1] < 0x10000 {
                    None
                } else {
                    Some(String::from_utf8_lossy(&read_cstr(memory, args[1])).into_owned())
                };
                let import = match &requested {
                    Some(name) => ImportName::ByName(name.clone()),
                    None => ImportName::Ordinal(args[1] as u16),
                };
                let address =
                    crate::win32::module::proc_address(ctx, memory, args[0], &import, 0);
                let address = address.or_else(|| {
                    let dll = crate::win32::kernel32::system_module_name(args[0])?;
                    // Functions of Rust-backed DLLs by name, or "#ordinal".
                    let name = requested.clone().unwrap_or_else(|| format!("#{}", args[1]));
                    crate::win32::kernel32::dynamic_thunk(ctx, dll, &name).or_else(|| {
                        crate::win32::nt::declared_delay_import(ctx, memory, dll, &name)
                            .then(|| crate::win32::kernel32::import_thunk(ctx, dll, &name))
                            .flatten()
                    })
                });
                match address {
                    Some(address) => ApiOutcome::new(address, count),
                    None => {
                        ctx.shared.borrow_mut().last_error = 127; // ERROR_PROC_NOT_FOUND
                        ApiOutcome::new(0, count)
                    }
                }
            }
            "LoadLibraryA" => {
                let name = String::from_utf8_lossy(&read_cstr(memory, args[0])).into_owned();
                if let Some(module) = ctx
                    .modules
                    .borrow().iter()
                    .find(|module| module.name.eq_ignore_ascii_case(&name))
                {
                    ApiOutcome::new(module.image_base, count)
                } else {
                    ctx.shared.borrow_mut().last_error = 126; // ERROR_MOD_NOT_FOUND
                    ApiOutcome::new(0, count)
                }
            }
            "SetErrorMode" => ApiOutcome::new(0, count),
            "GetCurrentThreadId" => ApiOutcome::new(1, count),
            "CloseHandle" | "WaitForSingleObject" => ApiOutcome::new(0, count),
            "CreateEventA" => {
                let handle = ctx.shared.borrow_mut().alloc_handle();
                ApiOutcome::new(handle, count)
            }

            "LocalAlloc" | "GlobalAlloc" => {
                let size = args[1];
                let address = ctx.shared.borrow_mut().bump_alloc(size);
                ApiOutcome::new(address, count)
            }
            "LocalLock" | "GlobalLock" => ApiOutcome::new(args[0], count),
            "LocalUnlock" | "GlobalUnlock" | "LocalFree" | "GlobalFree" => {
                ApiOutcome::new(1, count)
            }
            "LocalReAlloc" => ApiOutcome::new(args[0], count),

            "lstrlenA" => ApiOutcome::new(read_cstr(memory, args[0]).len() as u32, count),
            "lstrlenW" => ApiOutcome::new(wide_len(memory, args[0]), count),
            "MultiByteToWideChar" => {
                // An explicit length converts exactly that many bytes (any
                // terminator among them included); -1 converts through the
                // terminator.
                let mut source = if (args[3] as i32) < 0 {
                    read_cstr(memory, args[2])
                } else {
                    memory.read_bytes(args[2], args[3] as usize).unwrap_or_default()
                };
                if (args[3] as i32) < 0 {
                    source.push(0);
                }
                // UTF-8 (CP_UTF8) or the ANSI code page.
                let units: Vec<u16> = if args[0] == 65001 {
                    String::from_utf8_lossy(&source).encode_utf16().collect()
                } else {
                    crate::win32::unicode::ansi_to_wide(&source)
                };
                let needed = units.len() as u32;
                let capacity = args[5];
                let destination = args[4];
                if capacity == 0 {
                    ApiOutcome::new(needed, count)
                } else if capacity < needed || destination == 0 {
                    ctx.shared.borrow_mut().last_error = 122; // ERROR_INSUFFICIENT_BUFFER
                    ApiOutcome::new(0, count)
                } else {
                    let wide: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
                    let _ = memory.write_bytes(destination, &wide);
                    ApiOutcome::new(needed, count)
                }
            }
            "lstrcpyA" => {
                let text = read_cstr(memory, args[1]);
                if write_cstr(memory, args[0], &text).is_some() {
                    ApiOutcome::new(args[0], count)
                } else {
                    ApiOutcome::new(0, count)
                }
            }
            "lstrcatA" => {
                let mut text = read_cstr(memory, args[0]);
                text.extend_from_slice(&read_cstr(memory, args[1]));
                if write_cstr(memory, args[0], &text).is_some() {
                    ApiOutcome::new(args[0], count)
                } else {
                    ApiOutcome::new(0, count)
                }
            }
            "lstrcpynA" => {
                let max = args[2] as usize;
                let mut text = read_ansi(memory, args[1], max);
                if text.len() >= max.saturating_sub(1) {
                    text.truncate(max.saturating_sub(1));
                }
                if write_cstr(memory, args[0], &text).is_some() {
                    ApiOutcome::new(args[0], count)
                } else {
                    ApiOutcome::new(0, count)
                }
            }
            "lstrcmpA" => {
                let left = read_cstr(memory, args[0]);
                let right = read_cstr(memory, args[1]);
                ApiOutcome::new(cstr_cmp(&left, &right) as u32, count)
            }
            "lstrcmpiA" => {
                let left = read_cstr(memory, args[0]).to_ascii_lowercase();
                let right = read_cstr(memory, args[1]).to_ascii_lowercase();
                ApiOutcome::new(cstr_cmp(&left, &right) as u32, count)
            }

            "GetLocaleInfoA" => ApiOutcome::new(modern::get_locale_info(ctx, memory, args[1], args[2], args[3], false), count),
            "GetTimeFormatA" | "GetDateFormatA" => {
                if args[5] > 0 && args[4] != 0 {
                    let _ = write_cstr(memory, args[4], b"12:00");
                }
                ApiOutcome::new(5, count)
            }
            "GetLocalTime" => {
                // SYSTEMTIME is eight WORDs; Notepad only formats it on print paths.
                if args[0] != 0 {
                    let _ = memory.write_bytes(args[0], &[0u8; 16]);
                }
                ApiOutcome::new(0, count)
            }

            "CreateFileA" => {
                let handle = ctx.shared.borrow_mut().alloc_handle();
                ApiOutcome::new(handle, count)
            }
            "_lcreat" | "_lopen" => {
                let handle = ctx.shared.borrow_mut().alloc_handle();
                ApiOutcome::new(handle, count)
            }
            "_lclose" | "FindClose" | "DeleteFileA" => ApiOutcome::new(0, count),
            "_lwrite" => ApiOutcome::new(args[2], count),
            "_lread" | "_llseek" => ApiOutcome::new(0, count),
            "FindFirstFileA" => {
                ctx.shared.borrow_mut().last_error = 2;
                ApiOutcome::new(u32::MAX, count)
            }
            _ => return None,
        };
        Some(outcome)
    }

    fn wide_len<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> u32 {
        const MAX_WIDE_CHARS: u32 = 1 << 20;
        for index in 0..MAX_WIDE_CHARS {
            let Some(bytes) = memory.read_bytes(va.wrapping_add(index * 2), 2) else {
                return index;
            };
            if bytes.len() < 2 || bytes[0] == 0 && bytes[1] == 0 {
                return index;
            }
        }
        MAX_WIDE_CHARS
    }

    fn write_file<M: GuestMemory + ?Sized>(
        memory: &mut M,
        args: &[u32],
        count: usize,
    ) -> ApiOutcome {
        let handle = args[0];
        let buffer = args[1];
        let length = args[2];
        let bytes_written = args[3];
        if handle == 0 || length > 16 * 1024 * 1024 {
            return ApiOutcome::new(0, count);
        }
        let Some(bytes) = memory.read_bytes(buffer, length as usize) else {
            return ApiOutcome::new(0, count);
        };
        {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            if stdout
                .write_all(&bytes)
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return ApiOutcome::new(0, count);
            }
        }
        if bytes_written != 0 && memory.write_u32(bytes_written, length).is_none() {
            return ApiOutcome::new(0, count);
        }
        ApiOutcome::new(1, count)
    }
}

// ── SHELL32 ────────────────────────────────────────────────────────────────

mod shell32 {
    use super::*;

    pub(super) fn arg_count(name: &str) -> Option<usize> {
        Some(match name {
            "DragFinish" => 1,
            "DragAcceptFiles" => 2,
            "DragQueryFileA" => 4,
            "ShellAboutA" => 4,
            "ShellExecuteA" => 6,
            "Shell_NotifyIcon" | "Shell_NotifyIconA" => 2,
            _ => return None,
        })
    }

    pub(super) fn dispatch<M: GuestMemory + ?Sized>(
        ctx: &ApiContext,
        memory: &mut M,
        name: &ImportName,
        args: &[u32],
    ) -> Option<ApiOutcome> {
        let name = name_of(name)?;
        let count = arg_count(name)?;
        if args.len() < count {
            return None;
        }
        let value = match name {
            "Shell_NotifyIcon" | "Shell_NotifyIconA" => notify_icon(ctx, memory, args[0], args[1]),
            "DragQueryFileA" | "DragAcceptFiles" | "DragFinish" | "ShellAboutA" => 0,
            "ShellExecuteA" => 1,
            _ => return None,
        };
        Some(ApiOutcome::new(value, count))
    }
}

/// `Shell_NotifyIconA(message, NOTIFYICONDATA*)`: add, change or remove
/// an icon in the taskbar notification area.
fn notify_icon<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    message: u32,
    data: u32,
) -> u32 {
    const NIF_MESSAGE: u32 = 1;
    const NIF_ICON: u32 = 2;
    const NIF_TIP: u32 = 4;
    let read = |memory: &mut M, offset: u32| memory.read_u32(data + offset).unwrap_or(0);
    let (hwnd, id, flags) = (read(memory, 4), read(memory, 8), read(memory, 12));
    let callback = read(memory, 16);
    let icon = read(memory, 20);
    let tip = {
        let mut bytes = read_cstr(memory, data + 24);
        bytes.truncate(63);
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let mut shared = ctx.shared.borrow_mut();
    let tray = &mut shared.gui.tray;
    let index = tray.iter().position(|t| t.hwnd == hwnd && t.id == id);
    let result = match (message, index) {
        (0, None) => {
            let mut entry = TrayIcon {
                hwnd,
                id,
                ..TrayIcon::default()
            };
            if flags & NIF_MESSAGE != 0 {
                entry.callback = callback;
            }
            if flags & NIF_ICON != 0 {
                entry.icon = icon;
            }
            if flags & NIF_TIP != 0 {
                entry.tip = tip;
            }
            tray.push(entry);
            1
        }
        (1, Some(index)) => {
            let entry = &mut tray[index];
            if flags & NIF_MESSAGE != 0 {
                entry.callback = callback;
            }
            if flags & NIF_ICON != 0 {
                entry.icon = icon;
            }
            if flags & NIF_TIP != 0 {
                entry.tip = tip;
            }
            1
        }
        (2, Some(index)) => {
            tray.remove(index);
            1
        }
        _ => 0,
    };
    shared.gui.dirty = true;
    result
}

// ── SHLWAPI ────────────────────────────────────────────────────────────────

mod shlwapi {
    use super::*;

    pub(super) fn arg_count(name: &str) -> Option<usize> {
        Some(match name {
            "SHRegGetBoolUSValueA" => 4,
            "StrStrIA" => 2,
            _ => return None,
        })
    }

    pub(super) fn dispatch<M: GuestMemory + ?Sized>(
        _ctx: &ApiContext,
        memory: &mut M,
        name: &ImportName,
        args: &[u32],
    ) -> Option<ApiOutcome> {
        let name = name_of(name)?;
        let count = arg_count(name)?;
        if args.len() < count {
            return None;
        }

        let outcome = match name {
            "SHRegGetBoolUSValueA" => {
                // IE only reads optional UI flags here. The caller supplies the
                // default, which is the honest result when no registry exists.
                ApiOutcome::new(args[3], count)
            }
            "StrStrIA" => {
                let haystack = read_cstr(memory, args[0]);
                let needle = read_cstr(memory, args[1]);
                if needle.is_empty() {
                    return Some(ApiOutcome::new(args[0], count));
                }
                let found = haystack.windows(needle.len()).position(|window| {
                    window
                        .iter()
                        .zip(needle.iter())
                        .all(|(left, right)| left.eq_ignore_ascii_case(right))
                });
                match found {
                    Some(index) => ApiOutcome::new(args[0].wrapping_add(index as u32), count),
                    None => ApiOutcome::new(0, count),
                }
            }
            _ => return None,
        };
        Some(outcome)
    }
}

// ── COMDLG32 ───────────────────────────────────────────────────────────────

mod comdlg32 {
    use super::*;

    pub(super) fn arg_count(name: &str) -> Option<usize> {
        Some(match name {
            "CommDlgExtendedError" => 0,
            "GetSaveFileNameA" | "PageSetupDlgA" | "FindTextA" | "GetOpenFileNameA" => 1,
            "GetFileTitleA" => 3,
            _ => return None,
        })
    }

    pub(super) fn dispatch<M: GuestMemory + ?Sized>(
        _ctx: &ApiContext,
        memory: &mut M,
        name: &ImportName,
        args: &[u32],
    ) -> Option<ApiOutcome> {
        let name = name_of(name)?;
        let count = arg_count(name)?;
        if args.len() < count {
            return None;
        }
        let outcome = match name {
            "GetSaveFileNameA" | "GetOpenFileNameA" => ApiOutcome::new(
                crate::win32::controls::file_dialog(
                    _ctx,
                    memory,
                    args[0],
                    name == "GetSaveFileNameA",
                ),
                count,
            ),
            "PageSetupDlgA" | "FindTextA" => ApiOutcome::new(0, count),
            "GetFileTitleA" => {
                let source = read_cstr(memory, args[0]);
                let title = source
                    .rsplit(|byte| *byte == b'\\' || *byte == b'/')
                    .next()
                    .unwrap_or(&source)
                    .to_vec();
                let mut title = title;
                title.truncate(args[2].saturating_sub(1) as usize);
                let _ = write_cstr(memory, args[1], &title);
                ApiOutcome::new(title.len() as u32, count)
            }
            "CommDlgExtendedError" => ApiOutcome::new(0, count),
            _ => return None,
        };
        Some(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::is_rust_backed_dll;

    #[test]
    fn rust_backed_dlls_are_excluded_from_companion_mapping() {
        for name in ["WSOCK32.dll", "wsock32.DLL", "WINSOCK.DLL", "MSVCRT20.dll"] {
            assert!(is_rust_backed_dll(name), "{name} must stay Rust-backed");
        }
        for name in ["MFC30.DLL", "MFCO30.DLL", "USER32.dll", "HELLO.EXE"] {
            assert!(!is_rust_backed_dll(name), "{name} is not a Rust-backed DLL");
        }
    }
}
