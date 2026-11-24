#![allow(non_upper_case_globals)]
//! Menu bar app (AppKit through the raw Objective-C runtime — no extra crates).
//!
//! The status item opens a transient popover: clicks inside keep it open,
//! a click anywhere outside closes it.
//!
//! Everything here runs on the main thread. Audio runs on Core Audio's
//! real-time thread and only shares the lock-free `Params` with the UI.

use crate::au;
use crate::cf;
use crate::ffi::*;
use crate::hal;
use crate::pipeline::Pipeline;
use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vinylizer::params::{
    crackle_peak_db, Params, DEFAULT_PRESET, PARAM_INFO, PARAM_NAMES, PRESETS, RPM_LABELS,
};
use vinylizer::settings::{
    log, save_user_preset, support_dir, user_preset_dir, user_preset_path, user_presets as builtin_user_presets,
    Mode, Settings,
};

#[cfg(target_arch = "aarch64")]
const NO: ObjcBool = false;
#[cfg(not(target_arch = "aarch64"))]
const NO: ObjcBool = 0;

#[cfg(target_arch = "aarch64")]
const ALIGN_RIGHT: isize = 2;
#[cfg(not(target_arch = "aarch64"))]
const ALIGN_RIGHT: isize = 1;

#[cfg(target_arch = "aarch64")]
const BOOL_RET_TYPES: &CStr = c"B@:";
#[cfg(not(target_arch = "aarch64"))]
const BOOL_RET_TYPES: &CStr = c"c@:";

const OWN_BUNDLE: &str = "dev.vinylizer";
static TERMINATE: AtomicBool = AtomicBool::new(false);

// Popover geometry, points.
const W: f64 = 400.0;
const PAD: f64 = 16.0;
const FIELD_X: f64 = 120.0;
const SLIDER_X: f64 = 160.0;
const VALUE_W: f64 = 58.0;
const MAX_H: f64 = 720.0;

// Tags in the plugin preset pop-up.
const NO_PRESET: isize = -1;
const FACTORY_BASE: isize = 100_000;
// Tags in the built-in preset pop-up: shipped presets are 0.., the user's start here.
const USER_BASE: isize = 1_000;

struct AppEntry {
    root: String,
    name: String,
    playing: bool,
    running: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum AuKind {
    Slider,
    Toggle,
    Index,
}

/// One live control for a plugin parameter.
struct AuRow {
    kind: AuKind,
    control: Id,
    value: Id,
}

/// Controls of the currently built popover. All are owned by its view tree
/// and are replaced together on every rebuild.
struct Controls {
    status: Id,
    enabled: Id,
    preset: Id,
    preset_save: Id,
    preset_delete: Id,
    rpm: Id,
    bypass: Id,
    sliders: Vec<(Id, Id)>, // (slider, value label) per PARAM_NAMES entry; null for rpm
    au_preset: Id,
    au_save: Id,
    au_delete: Id,
    au_rows: Vec<AuRow>,
}

impl Default for Controls {
    fn default() -> Self {
        let null = ptr::null_mut();
        Self {
            status: null,
            enabled: null,
            preset: null,
            preset_save: null,
            preset_delete: null,
            rpm: null,
            bypass: null,
            sliders: Vec::new(),
            au_preset: null,
            au_save: null,
            au_delete: null,
            au_rows: Vec::new(),
        }
    }
}

struct Ui {
    pipe: Pipeline,
    cfg: Settings,
    cfg_path: PathBuf,
    controller: Id,
    flipped: Id,
    status_item: Id,
    popover: Id,
    scroll: Id,
    closed_at: Option<Instant>,
    effects: Vec<String>,
    apps: Vec<AppEntry>,
    c: Controls,
    /// The user's presets for the built-in effect: (name, values).
    builtin_user: Vec<(String, Vec<f32>)>,
    plugin_window: Id,
    au_presets: Vec<(i32, String)>,
    au_user_presets: Vec<(String, PathBuf)>,
    au_params: Vec<au::AuParam>,
    au_modified: bool,
    dirty: bool,
    ticks: u64,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// Run `f` with the UI state; silently skips re-entrant calls.
fn with_ui(f: impl FnOnce(&mut Ui)) {
    UI.with(|cell| {
        if let Ok(mut g) = cell.try_borrow_mut() {
            if let Some(ui) = g.as_mut() {
                f(ui);
            }
        }
    });
}

// ------------------------------------------------------------------ AppKit helpers

fn ns(s: &str) -> cf::Cf {
    cf::string(s)
}

unsafe fn ns_to_string(s: Id) -> String {
    cf::to_string(s as CFStringRef)
}

unsafe fn sel(name: &CStr) -> Sel {
    sel_registerName(name.as_ptr())
}

unsafe fn menu_item(title: &str, action: Option<&CStr>, target: Id, tag: isize) -> Id {
    let t = ns(title);
    let key = ns("");
    let s = action.map_or(ptr::null_mut(), |a| sel(a));
    let item = send!(class(c"NSMenuItem"), "alloc"; -> Id);
    let item = send!(item, "initWithTitle:action:keyEquivalent:", t.0 as Id => Id, s => Sel, key.0 as Id => Id; -> Id);
    if action.is_some() {
        send!(item, "setTarget:", target => Id; -> ());
    }
    send!(item, "setTag:", tag => isize; -> ());
    send!(item, "autorelease"; -> Id)
}

unsafe fn add(menu: Id, item: Id) -> Id {
    send!(menu, "addItem:", item => Id; -> ());
    item
}

unsafe fn separator(menu: Id) {
    let s = send!(class(c"NSMenuItem"), "separatorItem"; -> Id);
    add(menu, s);
}

unsafe fn set_state(item: Id, on: bool) {
    send!(item, "setState:", on as isize => isize; -> ());
}

unsafe fn set_enabled(item: Id, on: bool) {
    send!(item, "setEnabled:", if on { YES } else { NO } => ObjcBool; -> ());
}

unsafe fn set_text(field: Id, text: &str) {
    let t = ns(text);
    send!(field, "setStringValue:", t.0 as Id => Id; -> ());
}

unsafe fn set_tooltip(view: Id, text: &str) {
    let t = ns(text);
    send!(view, "setToolTip:", t.0 as Id => Id; -> ());
}

unsafe fn small_font() -> Id {
    send!(class(c"NSFont"), "systemFontOfSize:", 11.0f64 => f64; -> Id)
}

/// Text label; `secondary` uses the dimmed system colour.
unsafe fn text(s: &str, size: f64, bold: bool, secondary: bool) -> Id {
    let t = ns(s);
    let l = send!(class(c"NSTextField"), "labelWithString:", t.0 as Id => Id; -> Id);
    let font = if bold {
        send!(class(c"NSFont"), "boldSystemFontOfSize:", size => f64; -> Id)
    } else {
        send!(class(c"NSFont"), "systemFontOfSize:", size => f64; -> Id)
    };
    send!(l, "setFont:", font => Id; -> ());
    if secondary {
        let c = send!(class(c"NSColor"), "secondaryLabelColor"; -> Id);
        send!(l, "setTextColor:", c => Id; -> ());
    }
    send!(l, "setLineBreakMode:", 4isize => isize; -> ()); // truncate tail
    l
}

unsafe fn small(control: Id) {
    send!(control, "setControlSize:", 1isize => isize; -> ());
    send!(control, "setFont:", small_font() => Id; -> ());
}

unsafe fn popup(target: Id, action: &CStr, pulls_down: bool) -> Id {
    let p = send!(class(c"NSPopUpButton"), "alloc"; -> Id);
    let p = send!(p, "initWithFrame:pullsDown:", NSRect::default() => NSRect, if pulls_down { YES } else { NO } => ObjcBool; -> Id);
    send!(p, "setTarget:", target => Id; -> ());
    send!(p, "setAction:", sel(action) => Sel; -> ());
    small(p);
    send!(p, "autorelease"; -> Id)
}

unsafe fn popup_add(p: Id, title: &str, tag: isize) -> Id {
    let menu = send!(p, "menu"; -> Id);
    add(menu, menu_item(title, None, ptr::null_mut(), tag))
}

unsafe fn popup_separator(p: Id) {
    separator(send!(p, "menu"; -> Id));
}

unsafe fn popup_select_tag(p: Id, tag: isize) {
    send!(p, "selectItemWithTag:", tag => isize; -> ObjcBool);
}

/// Tag of the item the user picked in a pop-up.
unsafe fn selected_tag(p: Id) -> Option<isize> {
    let it = send!(p, "selectedItem"; -> Id);
    (!it.is_null()).then(|| tag_of(it))
}

unsafe fn button(title: &str, target: Id, action: &CStr) -> Id {
    let t = ns(title);
    let b = send!(class(c"NSButton"), "buttonWithTitle:target:action:", t.0 as Id => Id, target => Id, sel(action) => Sel; -> Id);
    small(b);
    b
}

unsafe fn checkbox(title: &str, on: bool, target: Id, action: &CStr) -> Id {
    let t = ns(title);
    let b = send!(class(c"NSButton"), "checkboxWithTitle:target:action:", t.0 as Id => Id, target => Id, sel(action) => Sel; -> Id);
    small(b);
    send!(b, "setState:", on as isize => isize; -> ());
    b
}

unsafe fn on_off_switch(on: bool, target: Id, action: &CStr) -> Id {
    let cls = class(c"NSSwitch");
    if cls.is_null() {
        return checkbox("", on, target, action);
    }

    let s = send!(send!(cls, "alloc"; -> Id), "initWithFrame:", NSRect::default() => NSRect; -> Id);
    send!(s, "setTarget:", target => Id; -> ());
    send!(s, "setAction:", sel(action) => Sel; -> ());
    send!(s, "setControlSize:", 1isize => isize; -> ());
    send!(s, "setState:", on as isize => isize; -> ());
    send!(s, "autorelease"; -> Id)
}

unsafe fn segmented(labels: &[&str], selected: isize, target: Id, action: &CStr) -> Id {
    let strings: Vec<cf::Cf> = labels.iter().map(|l| ns(l)).collect();
    let ids: Vec<Id> = strings.iter().map(|s| s.0 as Id).collect();
    let arr = send!(class(c"NSArray"), "arrayWithObjects:count:", ids.as_ptr() => *const Id, ids.len() => usize; -> Id);
    let s = send!(class(c"NSSegmentedControl"), "segmentedControlWithLabels:trackingMode:target:action:",
        arr => Id, 0usize => usize, target => Id, sel(action) => Sel; -> Id);
    small(s);
    send!(s, "setSelectedSegment:", selected => isize; -> ());
    s
}

unsafe fn hline() -> Id {
    let b = send!(send!(class(c"NSBox"), "alloc"; -> Id), "initWithFrame:", NSRect::default() => NSRect; -> Id);
    send!(b, "setBoxType:", 2usize => usize; -> ()); // separator
    send!(b, "autorelease"; -> Id)
}

unsafe fn ns_app() -> Id {
    send!(class(c"NSApplication"), "sharedApplication"; -> Id)
}

unsafe fn activate_app() {
    let app = ns_app();
    if send!(app, "respondsToSelector:", sel(c"activate") => Sel; -> ObjcBool) == YES {
        send!(app, "activate"; -> ());
    } else {
        send!(app, "activateIgnoringOtherApps:", YES => ObjcBool; -> ());
    }
}

unsafe fn new_alert(title: &str, info: &str, buttons: &[&str]) -> Id {
    activate_app();
    let alert = send!(send!(class(c"NSAlert"), "alloc"; -> Id), "init"; -> Id);
    let t = ns(title);
    let i = ns(info);
    send!(alert, "setMessageText:", t.0 as Id => Id; -> ());
    send!(alert, "setInformativeText:", i.0 as Id => Id; -> ());
    for b in buttons {
        let bt = ns(b);
        send!(alert, "addButtonWithTitle:", bt.0 as Id => Id; -> Id);
    }
    let win = send!(alert, "window"; -> Id);
    send!(win, "setLevel:", 3isize => isize; -> ());
    alert
}

/// Modal text prompt; `None` if cancelled or empty.
unsafe fn prompt(title: &str, info: &str, default: &str, ok: &str) -> Option<String> {
    let alert = new_alert(title, info, &[ok, "Отмена"]);
    let tf = send!(send!(class(c"NSTextField"), "alloc"; -> Id), "initWithFrame:", NSRect::new(0.0, 0.0, 280.0, 24.0) => NSRect; -> Id);
    let d = ns(default);
    send!(tf, "setStringValue:", d.0 as Id => Id; -> ());
    send!(alert, "setAccessoryView:", tf => Id; -> ());
    let win = send!(alert, "window"; -> Id);
    send!(win, "setInitialFirstResponder:", tf => Id; -> ());
    let r = send!(alert, "runModal"; -> isize);
    let text = ns_to_string(send!(tf, "stringValue"; -> Id));
    send!(tf, "release"; -> ());
    send!(alert, "release"; -> ());
    let text: String = text
        .trim()
        .chars()
        .map(|ch| if ch == '/' || ch == ':' { '-' } else { ch })
        .collect();
    (r == 1000 && !text.is_empty()).then_some(text)
}

unsafe fn confirm(title: &str, info: &str, ok: &str) -> bool {
    let alert = new_alert(title, info, &[ok, "Отмена"]);
    let r = send!(alert, "runModal"; -> isize);
    send!(alert, "release"; -> ());
    r == 1000
}

unsafe fn notify_error(title: &str, err: &str) {
    let alert = new_alert(title, err, &["OK"]);
    send!(alert, "runModal"; -> isize);
    send!(alert, "release"; -> ());
}

/// Move a file to the Trash (recoverable).
unsafe fn trash(path: &std::path::Path) -> bool {
    let p = ns(&path.to_string_lossy());
    let url = send!(class(c"NSURL"), "fileURLWithPath:", p.0 as Id => Id; -> Id);
    let fm = send!(class(c"NSFileManager"), "defaultManager"; -> Id);
    send!(fm, "trashItemAtURL:resultingItemURL:error:", url => Id, ptr::null_mut::<c_void>() => Id, ptr::null_mut::<c_void>() => Id; -> ObjcBool)
        == YES
}

/// Lays views out top to bottom in a flipped container.
struct Rows {
    view: Id,
    y: f64,
}

impl Rows {
    unsafe fn put(&self, v: Id, x: f64, dy: f64, w: f64, h: f64) {
        send!(v, "setFrame:", NSRect::new(x, self.y + dy, w, h) => NSRect; -> ());
        send!(self.view, "addSubview:", v => Id; -> ());
    }

    /// Caption on the left, control in the field column.
    unsafe fn field(&mut self, caption: &str, control: Id, w: f64) {
        self.put(text(caption, 12.0, false, false), PAD, 4.0, FIELD_X - PAD, 17.0);
        self.put(control, FIELD_X, 0.0, w, 24.0);
        self.y += 30.0;
    }

    /// A row of buttons of the given widths, left to right from `x`, with a gap between them.
    unsafe fn buttons(&mut self, x: f64, row: &[(Id, f64)]) {
        const GAP: f64 = 8.0;
        let mut x = x;
        for &(b, w) in row {
            self.put(b, x, 0.0, w, 24.0);
            x += w + GAP;
        }
        self.y += 30.0;
    }

    unsafe fn rule(&mut self) {
        self.y += 4.0;
        self.put(hline(), PAD, 0.0, W - 2.0 * PAD, 1.0);
        self.y += 9.0;
    }
}

// ------------------------------------------------------------------ app list

fn root_bundle(b: &str) -> &str {
    match b.to_lowercase().find(".helper") {
        Some(i) => &b[..i],
        None => b,
    }
}

unsafe fn app_display_name(root: &str) -> Option<String> {
    let b = ns(root);
    let arr = send!(class(c"NSRunningApplication"), "runningApplicationsWithBundleIdentifier:", b.0 as Id => Id; -> Id);
    if arr.is_null() || send!(arr, "count"; -> usize) == 0 {
        return None;
    }
    let app = send!(arr, "objectAtIndex:", 0usize => usize; -> Id);
    let name = send!(app, "localizedName"; -> Id);
    (!name.is_null())
        .then(|| ns_to_string(name))
        .filter(|n| !n.is_empty())
}

fn list_apps(selected: &[String]) -> Vec<AppEntry> {
    let mut out: Vec<AppEntry> = Vec::new();
    for p in hal::list_processes().unwrap_or_default() {
        if p.bundle.is_empty()
            || p.bundle.starts_with(OWN_BUNDLE)
            || p.pid == std::process::id() as i32
        {
            continue;
        }
        let root = root_bundle(&p.bundle).to_string();
        if let Some(e) = out.iter_mut().find(|e| e.root == root) {
            e.playing |= p.playing;
            continue;
        }
        let name = unsafe { app_display_name(&root) }
            .unwrap_or_else(|| root.rsplit('.').next().unwrap_or(&root).to_string());
        out.push(AppEntry {
            root,
            name,
            playing: p.playing,
            running: true,
        });
    }
    for t in selected {
        if !out.iter().any(|e| &e.root == t) {
            out.push(AppEntry {
                root: t.clone(),
                name: t.rsplit('.').next().unwrap_or(t).to_string(),
                playing: false,
                running: false,
            });
        }
    }
    out.sort_by(|a, b| {
        b.playing
            .cmp(&a.playing)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

fn split_component(component_name: &str) -> (&str, &str) {
    component_name
        .split_once(':')
        .map(|(m, p)| (m.trim(), p.trim()))
        .unwrap_or(("", component_name))
}

/// Where hosts keep user presets for this plugin.
fn preset_dir(component_name: &str) -> PathBuf {
    let (manu, plug) = split_component(component_name);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Audio/Presets").join(manu).join(plug)
}

/// `.aupreset` files saved by hosts (Logic, GarageBand, …) for this plugin:
/// `~/Library/Audio/Presets/<Manufacturer>/<Plugin>/` and the system-wide equivalent.
fn user_presets(component_name: &str) -> Vec<(String, PathBuf)> {
    let (manu, plug) = split_component(component_name);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for base in [
        home.join("Library/Audio/Presets"),
        PathBuf::from("/Library/Audio/Presets"),
    ] {
        let Ok(rd) = std::fs::read_dir(base.join(manu).join(plug)) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("aupreset") {
                let name = p
                    .file_stem()
                    .and_then(|x| x.to_str())
                    .unwrap_or("?")
                    .to_string();
                if !out.iter().any(|(n, _)| n == &name) {
                    out.push((name, p));
                }
            }
        }
    }
    let rank = |n: &str| {
        let l = n.to_lowercase();
        if l.contains("jazz") || l.contains("sinatra") {
            0
        } else {
            1
        }
    };
    out.sort_by(|a, b| {
        rank(&a.0)
            .cmp(&rank(&b.0))
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    out
}

/// Built-in parameter value as shown next to its slider.
fn format_builtin(name: &str, v: f32) -> String {
    match name {
        "gain" if v <= 0.001 => "−∞ dB".into(),
        "gain" => {
            let db = 20.0 * v.log10();
            if db.abs() < 0.05 {
                "0 dB".into()
            } else {
                format!("{db:+.1} dB")
            }
        }
        "width" if v < 0.005 => "моно".into(),
        "crackle_level" => format!("{:.0} dB", crackle_peak_db(v)),
        _ => format!("{:.0}%", v * 100.0),
    }
}

// ------------------------------------------------------------------ Ui logic

impl Ui {
    fn save(&mut self) {
        let _ = self.cfg.save(&self.cfg_path, &self.pipe.params);
        self.dirty = false;
    }

    fn apply(&mut self) {
        let current = self.pipe.au().map(|a| a.name.clone());
        // the plugin instance may be replaced: its editor must go first
        if current.as_deref() != self.cfg.active_au() {
            unsafe { self.close_plugin_window() };
        }

        let r = self.pipe.apply(&self.cfg);
        log(&format!(
            "apply enabled={} mode={:?} targets={:?} au={:?} -> {:?} | {} | au_loaded={:?}",
            self.cfg.enabled,
            self.cfg.mode,
            self.cfg.targets,
            self.cfg.au,
            r.as_ref().map(|_| ()),
            self.pipe.status,
            self.pipe.au().map(|a| a.name.clone())
        ));

        self.save();
        unsafe {
            self.update_icon();
            self.refresh_header();
        }
    }

    unsafe fn update_icon(&self) {
        let button = send!(self.status_item, "button"; -> Id);
        if button.is_null() {
            return;
        }
        let off = !self.pipe.is_running();
        send!(button, "setAppearsDisabled:", if off { YES } else { NO } => ObjcBool; -> ());
        let tip = ns(&format!("Vinylizer — {}", self.pipe.status));
        send!(button, "setToolTip:", tip.0 as Id => Id; -> ());
    }

    unsafe fn close_plugin_window(&mut self) {
        if !self.plugin_window.is_null() {
            send!(self.plugin_window, "close"; -> ());
            send!(self.plugin_window, "release"; -> ());
            self.plugin_window = ptr::null_mut();
        }
    }

    /// Bring a window to the front even though we are an accessory (menu bar) app.
    unsafe fn present(win: Id) {
        // floating level + follow the user's current Space
        send!(win, "setLevel:", 3isize => isize; -> ());
        send!(win, "setCollectionBehavior:", (1usize << 1) | (1usize << 8) => usize; -> ());
        send!(win, "makeKeyAndOrderFront:", ptr::null_mut::<c_void>() => Id; -> ());
        send!(win, "orderFrontRegardless"; -> ());
        activate_app();
    }

    unsafe fn open_plugin_window(&mut self) {
        if !self.plugin_window.is_null() {
            log("  re-presenting existing plugin window");
            Self::present(self.plugin_window);
            return;
        }
        log("open plugin window requested");
        let Some(plugin) = self.pipe.au() else {
            log("  no AU loaded");
            return;
        };
        let h = plugin.handle();
        let title = ns(&plugin.name);
        let mut view: Id = ptr::null_mut();

        // 1) the plugin's own Cocoa editor
        let cocoa = h.cocoa_ui();
        log(&format!("  cocoa_ui present: {}", cocoa.is_some()));
        if let Some((url, cls_name)) = cocoa {
            let url_s = send!(url as Id, "absoluteString"; -> Id);
            log(&format!(
                "  bundle={} class={}",
                ns_to_string(url_s),
                ns_to_string(cls_name as Id)
            ));
            let bundle = send!(class(c"NSBundle"), "bundleWithURL:", url as Id => Id; -> Id);
            let loaded = !bundle.is_null() && send!(bundle, "load"; -> ObjcBool) == YES;
            log(&format!("  bundle loaded={loaded}"));
            if loaded {
                let fcls = send!(bundle, "classNamed:", cls_name as Id => Id; -> Id);
                log(&format!("  factory class found={}", !fcls.is_null()));
                if !fcls.is_null() {
                    let factory = send!(send!(fcls, "alloc"; -> Id), "init"; -> Id);
                    if !factory.is_null() {
                        let size = NSSize {
                            width: 600.0,
                            height: 400.0,
                        };
                        view = send!(factory, "uiViewForAudioUnit:withSize:", h.0 => AudioUnit, size => NSSize; -> Id);
                        log(&format!("  plugin view created={}", !view.is_null()));
                        send!(factory, "release"; -> ());
                    }
                }
            }
            CFRelease(url);
            CFRelease(cls_name);
        }
        // 2) generic parameter editor from CoreAudioKit
        if view.is_null() {
            let g = class(c"AUGenericView");
            log(&format!(
                "  falling back to AUGenericView (class found={})",
                !g.is_null()
            ));
            if !g.is_null() {
                view =
                    send!(send!(g, "alloc"; -> Id), "initWithAudioUnit:", h.0 => AudioUnit; -> Id);
                if !view.is_null() {
                    send!(view, "autorelease"; -> Id);
                }
            }
        }
        if view.is_null() {
            log("  no view could be created");
            return;
        }

        #[cfg(target_arch = "aarch64")]
        let frame: NSRect = send!(view, "frame"; -> NSRect);
        #[cfg(not(target_arch = "aarch64"))]
        let frame = NSRect::new(0.0, 0.0, 600.0, 400.0);
        let (w, hgt) = if frame.size.width > 50.0 && frame.size.height > 50.0 {
            (frame.size.width, frame.size.height)
        } else {
            (600.0, 400.0)
        };

        // titled | closable | miniaturizable, buffered backing
        let win = send!(class(c"NSWindow"), "alloc"; -> Id);
        let win = send!(win, "initWithContentRect:styleMask:backing:defer:",
            NSRect::new(0.0, 0.0, w, hgt) => NSRect, 7usize => usize, 2usize => usize, NO => ObjcBool; -> Id);
        send!(win, "setReleasedWhenClosed:", NO => ObjcBool; -> ());
        send!(win, "setTitle:", title.0 as Id => Id; -> ());
        send!(win, "setContentView:", view => Id; -> ());
        send!(win, "center"; -> ());
        Self::present(win);
        let visible = send!(win, "isVisible"; -> ObjcBool) == YES;
        log(&format!(
            "  window created, size={w}x{hgt}, visible={visible}"
        ));
        self.plugin_window = win;
    }

    // ---------------------------------------------------------------- popover

    unsafe fn popover_shown(&self) -> bool {
        send!(self.popover, "isShown"; -> ObjcBool) == YES
    }

    unsafe fn toggle_popover(&mut self) {
        if self.popover_shown() {
            send!(self.popover, "performClose:", ptr::null_mut::<c_void>() => Id; -> ());
            return;
        }

        // The click on the icon that closed the popover arrives here right after.
        if self
            .closed_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(300))
        {
            return;
        }

        self.rebuild();
        let button = send!(self.status_item, "button"; -> Id);
        #[cfg(target_arch = "aarch64")]
        let bounds: NSRect = send!(button, "bounds"; -> NSRect);
        #[cfg(not(target_arch = "aarch64"))]
        let bounds = NSRect::new(0.0, 0.0, 24.0, 22.0);
        activate_app();
        send!(self.popover, "showRelativeToRect:ofView:preferredEdge:",
            bounds => NSRect, button => Id, 1usize => usize; -> ()); // below the icon
    }

    /// Rebuild later, outside the action of a control the rebuild would free.
    unsafe fn schedule_rebuild(&self) {
        send!(self.controller, "performSelector:withObject:afterDelay:",
            sel(c"rebuild:") => Sel, ptr::null_mut::<c_void>() => Id, 0.0f64 => f64; -> ());
    }

    /// Returns the content height.
    unsafe fn rebuild(&mut self) -> f64 {
        let doc = send!(self.flipped, "alloc"; -> Id);
        let doc = send!(doc, "initWithFrame:", NSRect::new(0.0, 0.0, W, 100.0) => NSRect; -> Id);
        self.c = Controls::default();
        let mut r = Rows { view: doc, y: 12.0 };
        self.build_header(&mut r);
        match self.cfg.mode {
            Mode::Builtin => self.build_builtin(&mut r),
            Mode::Plugin => self.build_plugin(&mut r),
        }
        r.rule();
        let quit = button("Выйти", self.controller, c"quit:");
        r.put(quit, W - PAD - 80.0, 0.0, 80.0, 24.0);
        r.y += 32.0;

        let h = r.y;
        send!(doc, "setFrameSize:", NSSize { width: W, height: h } => NSSize; -> ());
        send!(self.scroll, "setDocumentView:", doc => Id; -> ());
        send!(doc, "release"; -> ());
        send!(self.popover, "setContentSize:", NSSize { width: W, height: h.min(MAX_H) } => NSSize; -> ());
        let clip = send!(self.scroll, "contentView"; -> Id);
        send!(clip, "scrollToPoint:", NSPoint { x: 0.0, y: 0.0 } => NSPoint; -> ());
        send!(self.scroll, "reflectScrolledClipView:", clip => Id; -> ());
        h
    }

    unsafe fn build_header(&mut self, r: &mut Rows) {
        let c = self.controller;

        r.put(text("Vinylizer", 13.0, true, false), PAD, 0.0, 200.0, 18.0);
        let sw = on_off_switch(self.cfg.enabled, c, c"toggleEnabled:");
        set_tooltip(sw, "Включить или выключить обработку звука");
        r.put(sw, W - PAD - 38.0, -1.0, 38.0, 20.0);
        self.c.enabled = sw;
        r.y += 21.0;
        let st = text(&self.pipe.status, 11.0, false, true);
        r.put(st, PAD, 0.0, W - 2.0 * PAD, 15.0);
        self.c.status = st;
        r.y += 18.0;
        r.rule();

        // ---- which apps
        self.apps = list_apps(&self.cfg.targets);
        let chosen: Vec<&str> = self
            .apps
            .iter()
            .filter(|a| self.cfg.targets.contains(&a.root))
            .map(|a| a.name.as_str())
            .collect();
        let title = if self.cfg.targets.is_empty() {
            "Все приложения".to_string()
        } else {
            chosen.join(", ")
        };
        let p = popup(c, c"selectTarget:", true);
        popup_add(p, &title, -2);
        set_state(popup_add(p, "Все приложения", -1), self.cfg.targets.is_empty());
        popup_separator(p);
        if self.apps.is_empty() {
            set_enabled(popup_add(p, "Никто не играет звук", -3), false);
        }
        for (i, a) in self.apps.iter().enumerate() {
            let t = match (a.playing, a.running) {
                (true, _) => format!("{}   ♪", a.name),
                (false, false) => format!("{}   (не запущено)", a.name),
                _ => a.name.clone(),
            };
            set_state(popup_add(p, &t, i as isize), self.cfg.targets.contains(&a.root));
        }
        r.field("Применять к", p, W - FIELD_X - PAD);

        // ---- mode
        let mode = match self.cfg.mode {
            Mode::Builtin => 0,
            Mode::Plugin => 1,
        };
        let seg = segmented(&["Vinylizer", "Сторонний плагин"], mode, c, c"selectMode:");
        r.field("Режим", seg, W - FIELD_X - PAD);
        r.rule();
    }

    unsafe fn build_builtin(&mut self, r: &mut Rows) {
        let c = self.controller;
        let params = self.pipe.params.clone();

        let p = popup(c, c"selectPreset:", false);
        for (i, (_, label, _)) in PRESETS.iter().enumerate() {
            popup_add(p, label, i as isize);
        }
        self.builtin_user = builtin_user_presets(&user_preset_dir());
        if !self.builtin_user.is_empty() {
            popup_separator(p);
        }
        for (i, (name, _)) in self.builtin_user.iter().enumerate() {
            popup_add(p, name, USER_BASE + i as isize);
        }
        popup_separator(p);
        popup_add(p, "Свой (не сохранён)", -1);
        self.c.preset = p;
        let bp = checkbox("Без эффекта", params.bypass.load(Relaxed), c, c"toggleBypass:");
        set_tooltip(bp, "Сравнить с оригиналом: звук идёт мимо эффекта, пока галочка стоит");
        r.put(bp, W - PAD - 96.0, 3.0, 96.0, 18.0);
        self.c.bypass = bp;
        r.field("Пресет", p, W - FIELD_X - PAD - 104.0);

        let save = button("Сохранить", c, c"saveBuiltinPreset:");
        set_tooltip(save, "Перезаписать свой пресет текущими настройками");
        let save_as = button("Сохранить как…", c, c"saveBuiltinPresetAs:");
        set_tooltip(save_as, "Сохранить текущие настройки под новым именем");
        let del = button("Удалить…", c, c"deleteBuiltinPreset:");
        let reset = button("Сбросить", c, c"resetBuiltin:");
        set_tooltip(reset, "Вернуть настройки по умолчанию (пресет «Тёплая»)");
        r.buttons(PAD, &[(save, 84.0), (save_as, 110.0), (del, 76.0), (reset, 74.0)]);
        self.c.preset_save = save;
        self.c.preset_delete = del;

        let rp = popup(c, c"selectRpm:", false);
        for (i, l) in RPM_LABELS.iter().enumerate() {
            popup_add(rp, l, i as isize);
        }
        set_tooltip(rp, PARAM_INFO[PARAM_NAMES.len() - 1].2);
        self.c.rpm = rp;
        r.field("Скорость", rp, 140.0);
        r.y += 4.0;

        self.c.sliders = vec![(ptr::null_mut(), ptr::null_mut()); PARAM_NAMES.len()];
        for (i, (key, name, hint)) in PARAM_INFO.iter().enumerate() {
            if *key == "rpm" {
                continue;
            }
            let v = params.get(key).unwrap_or(0.0);
            let max = if *key == "gain" { 2.0 } else { 1.0 };
            self.c.sliders[i] = self.slider_row(
                r,
                i,
                name,
                Some(hint),
                v as f64,
                0.0,
                max,
                &format_builtin(key, v),
                c"sliderChanged:",
            );
        }

        // Values that equal a user preset mean that preset is loaded, so later edits can be saved over it.
        if self.current_user_preset().is_none() && self.pipe.params.matching_preset().is_none() {
            if let Some(i) = self.matching_user_preset() {
                self.cfg.preset = Some(self.builtin_user[i].0.clone());
                self.dirty = true;
            }
        }
        self.refresh_builtin();
    }

    unsafe fn build_plugin(&mut self, r: &mut Rows) {
        let c = self.controller;

        let p = popup(c, c"selectEffect:", false);
        if self.cfg.au.is_none() {
            popup_add(p, "Выберите плагин", -1);
        }
        for (i, n) in self.effects.iter().enumerate() {
            popup_add(p, n, i as isize);
        }
        if self.effects.is_empty() {
            set_enabled(popup_add(p, "Нет установленных AU-эффектов", -2), false);
        }
        let sel_tag = self
            .cfg
            .au
            .as_ref()
            .and_then(|a| self.effects.iter().position(|e| e == a))
            .map_or(-1, |i| i as isize);
        popup_select_tag(p, sel_tag);
        let win = button("Окно плагина…", c, c"openPluginUI:");
        set_enabled(win, self.pipe.au().is_some());
        r.put(win, W - PAD - 110.0, 0.0, 110.0, 24.0);
        r.field("Плагин", p, W - FIELD_X - PAD - 116.0);

        let Some(plugin) = self.pipe.au() else {
            let why = if self.cfg.au.is_none() {
                "Выберите плагин из списка выше"
            } else if !self.cfg.enabled {
                "Плагин загрузится, когда Vinylizer включён"
            } else {
                "Плагин не загружен — см. статус вверху"
            };
            r.put(text(why, 11.0, false, true), PAD, 0.0, W - 2.0 * PAD, 15.0);
            r.y += 20.0;
            return;
        };
        let h = plugin.handle();
        let plugin_name = plugin.name.clone();

        // ---- presets: the plugin's own (factory) and the ones hosts saved for it
        self.au_user_presets = user_presets(&plugin_name);
        self.au_presets = h.factory_presets();
        let only_default = self.au_presets.len() == 1;
        self.au_presets.retain(|p| p.1 != "Default" || only_default);
        let pp = popup(c, c"selectAuPreset:", false);
        popup_add(pp, "—", NO_PRESET);
        for (i, (name, _)) in self.au_user_presets.iter().enumerate() {
            popup_add(pp, name, i as isize);
        }
        if !self.au_presets.is_empty() {
            popup_separator(pp);
        }
        for (num, name) in &self.au_presets {
            popup_add(pp, name, FACTORY_BASE + *num as isize);
        }
        self.c.au_preset = pp;
        r.field("Пресет", pp, W - FIELD_X - PAD);

        let save = button("Сохранить", c, c"savePreset:");
        let save_as = button("Сохранить как…", c, c"savePresetAs:");
        let del = button("Удалить…", c, c"deletePreset:");
        let reset = button("Сбросить", c, c"resetPlugin:");
        set_tooltip(reset, "Вернуть плагину настройки по умолчанию");
        r.buttons(PAD, &[(save, 84.0), (save_as, 110.0), (del, 76.0), (reset, 74.0)]);
        self.c.au_save = save;
        self.c.au_delete = del;
        r.rule();

        // ---- live parameters
        self.au_params = h
            .params()
            .into_iter()
            .filter(|p| p.flags & kAudioUnitParameterFlag_IsWritable != 0 && p.max > p.min)
            .collect();
        if self.au_params.is_empty() {
            r.put(text("Нет изменяемых параметров", 11.0, false, true), PAD, 0.0, 300.0, 15.0);
            r.y += 20.0;
        }
        for i in 0..self.au_params.len() {
            let p = self.au_params[i].clone();
            let v = h.get_param(p.id);
            let row = match p.unit {
                kAudioUnitParameterUnit_Boolean => {
                    let b = checkbox(&p.name, v >= 0.5, c, c"auToggle:");
                    send!(b, "setTag:", i as isize => isize; -> ());
                    r.put(b, PAD, 0.0, W - 2.0 * PAD, 18.0);
                    r.y += 24.0;
                    AuRow {
                        kind: AuKind::Toggle,
                        control: b,
                        value: ptr::null_mut(),
                    }
                }
                kAudioUnitParameterUnit_Indexed => {
                    let names = h.value_strings(p.id);
                    let pop = popup(c, c"auIndex:", false);
                    send!(pop, "setTag:", i as isize => isize; -> ());
                    let lo = p.min.round() as i64;
                    let hi = p.max.round() as i64;
                    for k in lo..=hi {
                        let label = names
                            .get((k - lo) as usize)
                            .cloned()
                            .unwrap_or_else(|| format!("{k}"));
                        popup_add(pop, &label, (k - lo) as isize);
                    }
                    r.put(text(&p.name, 12.0, false, false), PAD, 4.0, SLIDER_X - PAD, 17.0);
                    r.put(pop, SLIDER_X, 0.0, W - SLIDER_X - PAD, 24.0);
                    r.y += 28.0;
                    AuRow {
                        kind: AuKind::Index,
                        control: pop,
                        value: ptr::null_mut(),
                    }
                }
                _ => {
                    let (slider, value) = self.slider_row(
                        r,
                        i,
                        &p.name,
                        None,
                        v as f64,
                        p.min as f64,
                        p.max as f64,
                        &p.format(v),
                        c"auSliderChanged:",
                    );
                    AuRow {
                        kind: AuKind::Slider,
                        control: slider,
                        value,
                    }
                }
            };
            self.c.au_rows.push(row);
        }
        self.refresh_au();
    }

    /// Row: name | slider | value, with an optional hint line underneath.
    /// Returns (slider, value label).
    #[allow(clippy::too_many_arguments)]
    unsafe fn slider_row(
        &mut self,
        r: &mut Rows,
        tag: usize,
        name: &str,
        hint: Option<&str>,
        value: f64,
        min: f64,
        max: f64,
        shown: &str,
        action: &CStr,
    ) -> (Id, Id) {
        let name_l = text(name, 12.0, false, false);
        r.put(name_l, PAD, 2.0, SLIDER_X - PAD - 4.0, 17.0);
        let slider = send!(class(c"NSSlider"), "sliderWithValue:minValue:maxValue:target:action:",
            value => f64, min => f64, max => f64, self.controller => Id, sel(action) => Sel; -> Id);
        send!(slider, "setTag:", tag as isize => isize; -> ());
        send!(slider, "setContinuous:", YES => ObjcBool; -> ());
        send!(slider, "setControlSize:", 1isize => isize; -> ());
        r.put(slider, SLIDER_X, 0.0, W - SLIDER_X - PAD - VALUE_W - 4.0, 20.0);
        let val_l = text(shown, 11.0, false, false);
        send!(val_l, "setAlignment:", ALIGN_RIGHT => isize; -> ());
        r.put(val_l, W - PAD - VALUE_W, 3.0, VALUE_W, 15.0);

        match hint {
            Some(h) => {
                set_tooltip(name_l, h);
                set_tooltip(slider, h);
                r.put(text(h, 10.5, false, true), PAD, 19.0, W - 2.0 * PAD, 14.0);
                r.y += 40.0;
            }
            None => r.y += 26.0,
        }
        (slider, val_l)
    }

    unsafe fn refresh_header(&self) {
        if !self.c.status.is_null() {
            set_text(self.c.status, &self.pipe.status);
        }
        if !self.c.enabled.is_null() {
            send!(self.c.enabled, "setState:", self.cfg.enabled as isize => isize; -> ());
        }
    }

    /// Push the current built-in values into the controls (after a preset change).
    unsafe fn refresh_builtin(&self) {
        if self.c.preset.is_null() {
            return;
        }

        let params = &self.pipe.params;
        for (i, n) in PARAM_NAMES.iter().enumerate() {
            let (slider, label) = self.c.sliders[i];
            if slider.is_null() {
                continue;
            }
            let v = params.get(n).unwrap_or(0.0);
            send!(slider, "setDoubleValue:", v as f64 => f64; -> ());
            set_text(label, &format_builtin(n, v));
        }
        popup_select_tag(self.c.rpm, params.rpm_index() as isize);
        send!(self.c.bypass, "setState:", params.bypass.load(Relaxed) as isize => isize; -> ());
        self.refresh_preset_choice();
    }

    /// The user preset the current values match, if any.
    fn matching_user_preset(&self) -> Option<usize> {
        self.builtin_user
            .iter()
            .position(|(_, vals)| self.pipe.params.matches(vals))
    }

    /// The user preset last loaded or saved, if it still exists.
    fn current_user_preset(&self) -> Option<usize> {
        let name = self.cfg.preset.as_deref()?;
        self.builtin_user.iter().position(|(n, _)| n == name)
    }

    /// The user preset that Save overwrites and Delete removes.
    fn selected_user_preset(&self) -> Option<usize> {
        self.current_user_preset()
            .or_else(|| self.matching_user_preset())
    }

    unsafe fn refresh_preset_choice(&self) {
        if self.c.preset.is_null() {
            return;
        }

        // The loaded user preset stays selected while edited; otherwise a shipped preset
        // wins over a user one holding the same values.
        let current = self.current_user_preset();
        let edited = current.is_some_and(|i| !self.pipe.params.matches(&self.builtin_user[i].1));
        let tag = match (current, self.pipe.params.matching_preset(), self.matching_user_preset()) {
            (Some(i), _, _) => USER_BASE + i as isize,
            (None, Some(i), _) => i as isize,
            (None, None, Some(i)) => USER_BASE + i as isize,
            (None, None, None) => -1,
        };

        let menu = send!(self.c.preset, "menu"; -> Id);
        for (i, (name, _)) in self.builtin_user.iter().enumerate() {
            let it = send!(menu, "itemWithTag:", USER_BASE + i as isize => isize; -> Id);
            let t = ns(&if edited && current == Some(i) {
                format!("{name} (изменён)")
            } else {
                name.clone()
            });
            send!(it, "setTitle:", t.0 as Id => Id; -> ());
        }
        popup_select_tag(self.c.preset, tag);

        set_enabled(self.c.preset_save, edited);
        set_enabled(self.c.preset_delete, self.selected_user_preset().is_some());
    }

    /// Push the plugin's current values into the controls (after a preset load).
    unsafe fn refresh_au(&self) {
        let Some(plugin) = self.pipe.au() else { return };
        let h = plugin.handle();

        for (row, p) in self.c.au_rows.iter().zip(&self.au_params) {
            let v = h.get_param(p.id);
            match row.kind {
                AuKind::Slider => {
                    send!(row.control, "setDoubleValue:", v as f64 => f64; -> ());
                    set_text(row.value, &p.format(v));
                }
                AuKind::Toggle => {
                    send!(row.control, "setState:", (v >= 0.5) as isize => isize; -> ());
                }
                AuKind::Index => {
                    popup_select_tag(row.control, (v.round() - p.min.round()) as isize);
                }
            }
        }
        self.refresh_au_preset();
    }

    /// Preset pop-up selection, "edited" mark, Save/Delete availability.
    unsafe fn refresh_au_preset(&self) {
        let pp = self.c.au_preset;
        if pp.is_null() {
            return;
        }

        let cur = self.cfg.au_preset.as_deref();
        let user_idx = cur.and_then(|n| self.au_user_presets.iter().position(|p| p.0 == n));
        let factory = cur.and_then(|n| self.au_presets.iter().find(|p| p.1 == n));
        let tag = match (user_idx, factory) {
            (Some(i), _) => i as isize,
            (None, Some((num, _))) => FACTORY_BASE + *num as isize,
            _ => NO_PRESET,
        };

        // Item titles carry the "edited" mark of the selected one only.
        let menu = send!(pp, "menu"; -> Id);
        for (i, (name, _)) in self.au_user_presets.iter().enumerate() {
            let it = send!(menu, "itemWithTag:", i as isize => isize; -> Id);
            let t = ns(&if self.au_modified && i as isize == tag {
                format!("{name} (изменён)")
            } else {
                name.clone()
            });
            send!(it, "setTitle:", t.0 as Id => Id; -> ());
        }
        for (num, name) in &self.au_presets {
            let ftag = FACTORY_BASE + *num as isize;
            let it = send!(menu, "itemWithTag:", ftag => isize; -> Id);
            let t = ns(&if self.au_modified && ftag == tag {
                format!("{name} (изменён)")
            } else {
                name.clone()
            });
            send!(it, "setTitle:", t.0 as Id => Id; -> ());
        }
        popup_select_tag(pp, tag);

        set_enabled(self.c.au_save, user_idx.is_some() && self.au_modified);
        set_enabled(self.c.au_delete, user_idx.is_some());
    }

    /// Write the plugin's current state as `<name>.aupreset` in the user preset folder.
    fn write_preset(&mut self, name: &str) -> Result<(), String> {
        let a = self.pipe.au().ok_or("плагин не загружен")?;
        let bytes = a
            .handle()
            .export_preset(name)
            .ok_or("плагин не отдал своё состояние")?;
        let dir = preset_dir(&a.name);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(format!("{name}.aupreset")), bytes).map_err(|e| e.to_string())?;
        self.cfg.au_preset = Some(name.to_string());
        self.au_modified = false;
        self.pipe.save_au_state();
        self.save();
        Ok(())
    }

    fn shutdown(&mut self) {
        unsafe { self.close_plugin_window() };
        self.save();
        self.pipe.stop();
        self.pipe.save_au_state();
    }
}

// ------------------------------------------------------------------ ObjC callbacks

unsafe fn tag_of(sender: Id) -> isize {
    send!(sender, "tag"; -> isize)
}

extern "C" fn is_flipped(_: Id, _: Sel) -> ObjcBool {
    YES
}

extern "C" fn on_toggle_popover(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe { ui.toggle_popover() });
}

extern "C" fn on_popover_closed(_: Id, _: Sel, _: Id) {
    with_ui(|ui| ui.closed_at = Some(Instant::now()));
}

extern "C" fn on_rebuild(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe {
        ui.rebuild();
    });
}

extern "C" fn on_toggle_enabled(_: Id, _: Sel, sender: Id) {
    let on = unsafe { send!(sender, "state"; -> isize) } != 0;
    with_ui(|ui| {
        let had_au = ui.pipe.au().is_some();
        ui.cfg.enabled = on;
        ui.apply();
        if ui.pipe.au().is_some() != had_au {
            unsafe { ui.schedule_rebuild() };
        }
    });
}

extern "C" fn on_select_target(_: Id, _: Sel, sender: Id) {
    let Some(tag) = (unsafe { selected_tag(sender) }) else {
        return;
    };
    with_ui(|ui| {
        if tag == -1 {
            ui.cfg.targets.clear();
        } else if let Some(a) = ui.apps.get(tag as usize) {
            if let Some(pos) = ui.cfg.targets.iter().position(|t| t == &a.root) {
                ui.cfg.targets.remove(pos);
            } else {
                ui.cfg.targets.push(a.root.clone());
            }
        } else {
            return;
        }
        ui.apply();
        unsafe { ui.schedule_rebuild() };
    });
}

extern "C" fn on_select_mode(_: Id, _: Sel, sender: Id) {
    let seg = unsafe { send!(sender, "selectedSegment"; -> isize) };
    with_ui(|ui| {
        ui.cfg.mode = if seg == 1 { Mode::Plugin } else { Mode::Builtin };
        ui.cfg.chain = false;
        // First time in plugin mode: take the first vinyl-like effect.
        if ui.cfg.mode == Mode::Plugin && ui.cfg.au.is_none() {
            ui.cfg.au = ui.effects.first().cloned();
        }
        ui.apply();
        unsafe { ui.schedule_rebuild() };
    });
}

extern "C" fn on_select_effect(_: Id, _: Sel, sender: Id) {
    let Some(tag) = (unsafe { selected_tag(sender) }) else {
        return;
    };
    with_ui(|ui| {
        let Some(name) = ui.effects.get(tag as usize).cloned() else {
            return;
        };
        if ui.cfg.au.as_deref() != Some(&name) {
            ui.cfg.au = Some(name);
            ui.cfg.au_preset = None;
            ui.au_modified = false;
        }
        ui.apply();
        unsafe { ui.schedule_rebuild() };
    });
}

extern "C" fn on_toggle_bypass(_: Id, _: Sel, sender: Id) {
    let on = unsafe { send!(sender, "state"; -> isize) } != 0;
    with_ui(|ui| ui.pipe.params.bypass.store(on, Relaxed));
}

extern "C" fn on_select_preset(_: Id, _: Sel, sender: Id) {
    let Some(tag) = (unsafe { selected_tag(sender) }) else {
        return;
    };
    with_ui(|ui| unsafe {
        if tag >= USER_BASE {
            if let Some((name, vals)) = ui.builtin_user.get((tag - USER_BASE) as usize).cloned() {
                ui.pipe.params.apply_values(&vals);
                ui.cfg.preset = Some(name);
                ui.save();
            }
        } else if tag >= 0 {
            if let Some((n, _, _)) = PRESETS.get(tag as usize) {
                let _ = ui.pipe.params.apply_preset(n);
                ui.cfg.preset = None;
                ui.save();
            }
        }
        ui.refresh_builtin();
    });
}

/// Overwrite the current user preset with the current values.
extern "C" fn on_save_builtin_preset(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe {
        let Some(i) = ui.current_user_preset() else {
            return;
        };
        let name = ui.builtin_user[i].0.clone();

        let r = save_user_preset(&user_preset_dir(), &name, &ui.pipe.params);
        log(&format!("overwrite built-in preset {name}: {r:?}"));
        match r {
            Ok(()) => {
                ui.builtin_user[i].1 = ui.pipe.params.values();
                ui.refresh_preset_choice();
            }
            Err(e) => notify_error("Не удалось сохранить пресет", &e.to_string()),
        }
    });
}

extern "C" fn on_save_builtin_preset_as(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe {
        let dir = user_preset_dir();
        let default = ui
            .selected_user_preset()
            .map(|i| ui.builtin_user[i].0.clone())
            .unwrap_or_else(|| "Мой пресет".into());
        let Some(name) = prompt(
            "Сохранить пресет Vinylizer",
            &format!("Текущие настройки сохранятся в {}.", dir.display()),
            &default,
            "Сохранить",
        ) else {
            return;
        };

        if user_preset_path(&dir, &name).exists()
            && !confirm(
                &format!("Заменить \u{201c}{name}\u{201d}?"),
                "Пресет с таким именем уже есть.",
                "Заменить",
            )
        {
            return;
        }

        let r = save_user_preset(&dir, &name, &ui.pipe.params);
        log(&format!("save built-in preset {name}: {r:?}"));
        match r {
            Ok(()) => {
                ui.cfg.preset = Some(name);
                ui.save();
                ui.schedule_rebuild();
            }
            Err(e) => notify_error("Не удалось сохранить пресет", &e.to_string()),
        }
    });
}

extern "C" fn on_delete_builtin_preset(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe {
        let Some(i) = ui.selected_user_preset() else {
            return;
        };
        let name = ui.builtin_user[i].0.clone();
        let path = user_preset_path(&user_preset_dir(), &name);
        if !confirm(
            &format!("Удалить пресет \u{201c}{name}\u{201d}?"),
            "Файл будет перемещён в Корзину. Текущие настройки останутся как есть.",
            "В Корзину",
        ) {
            return;
        }

        let ok = trash(&path);
        log(&format!("delete built-in preset {name}: {ok}"));
        if ok {
            ui.cfg.preset = None;
            ui.save();
            ui.schedule_rebuild();
        } else {
            notify_error("Не удалось удалить пресет", &path.display().to_string());
        }
    });
}

extern "C" fn on_reset_builtin(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe {
        let _ = ui.pipe.params.apply_preset(DEFAULT_PRESET);
        ui.pipe.params.bypass.store(false, Relaxed);
        ui.cfg.preset = None;
        ui.save();
        ui.refresh_builtin();
    });
}

extern "C" fn on_reset_plugin(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe {
        let Some(a) = ui.pipe.au() else { return };
        let h = a.handle();

        // The plugin's own "Default" preset knows about state the parameters don't show.
        let factory_default = h.factory_presets().into_iter().find(|p| p.1 == "Default");
        let r = match &factory_default {
            Some((num, _)) => h.set_preset(*num),
            None => {
                for p in h.params() {
                    if p.flags & kAudioUnitParameterFlag_IsWritable != 0 {
                        let _ = h.set_param(p.id, p.default);
                    }
                }
                Ok(())
            }
        };
        log(&format!("reset plugin to defaults: {r:?}"));

        ui.cfg.au_preset = factory_default.map(|p| p.1);
        ui.au_modified = false;
        ui.save();
        ui.pipe.save_au_state();
        ui.refresh_au();
    });
}

extern "C" fn on_select_rpm(_: Id, _: Sel, sender: Id) {
    let Some(tag) = (unsafe { selected_tag(sender) }) else {
        return;
    };
    with_ui(|ui| unsafe {
        let _ = ui.pipe.params.set("rpm", tag as f32);
        ui.dirty = true;
        ui.refresh_preset_choice();
    });
}

extern "C" fn on_slider(_: Id, _: Sel, sender: Id) {
    unsafe {
        let idx = tag_of(sender) as usize;
        let v = send!(sender, "doubleValue"; -> f64) as f32;
        with_ui(|ui| {
            let Some(n) = PARAM_NAMES.get(idx) else { return };
            let _ = ui.pipe.params.set(n, v);
            if let Some(&(_, l)) = ui.c.sliders.get(idx) {
                if !l.is_null() {
                    set_text(l, &format_builtin(n, v));
                }
            }
            ui.refresh_preset_choice();
            ui.dirty = true;
        });
    }
}

extern "C" fn on_select_au_preset(_: Id, _: Sel, sender: Id) {
    let Some(tag) = (unsafe { selected_tag(sender) }) else {
        return;
    };
    with_ui(|ui| unsafe {
        let Some(a) = ui.pipe.au() else { return };
        let h = a.handle();

        let (name, r) = if tag >= FACTORY_BASE {
            let num = (tag - FACTORY_BASE) as i32;
            let name = ui.au_presets.iter().find(|p| p.0 == num).map(|p| p.1.clone());
            (name, h.set_preset(num))
        } else if let Some((name, path)) = ui.au_user_presets.get(tag as usize).cloned() {
            let r = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|b| h.load_state(&b));
            (Some(name), r)
        } else {
            ui.refresh_au_preset();
            return;
        };
        log(&format!("load AU preset {name:?}: {r:?}"));

        if r.is_ok() {
            ui.cfg.au_preset = name;
            ui.au_modified = false;
            ui.save();
            ui.pipe.save_au_state();
        }
        ui.refresh_au();
    });
}

/// After a parameter edit: mark the preset as edited.
unsafe fn au_edited(ui: &mut Ui) {
    if !ui.au_modified {
        ui.au_modified = true;
        ui.refresh_au_preset();
    }
}

extern "C" fn on_au_slider(_: Id, _: Sel, sender: Id) {
    unsafe {
        let idx = tag_of(sender) as usize;
        let v = send!(sender, "doubleValue"; -> f64) as f32;
        with_ui(|ui| {
            let (Some(p), Some(a)) = (ui.au_params.get(idx).cloned(), ui.pipe.au()) else {
                return;
            };
            let _ = a.handle().set_param(p.id, v);
            if let Some(row) = ui.c.au_rows.get(idx) {
                set_text(row.value, &p.format(v));
            }
            au_edited(ui);
        });
    }
}

extern "C" fn on_au_toggle(_: Id, _: Sel, sender: Id) {
    let (idx, on) = unsafe { (tag_of(sender) as usize, send!(sender, "state"; -> isize) != 0) };
    with_ui(|ui| unsafe {
        let (Some(p), Some(a)) = (ui.au_params.get(idx).cloned(), ui.pipe.au()) else {
            return;
        };
        let _ = a.handle().set_param(p.id, if on { p.max } else { p.min });
        au_edited(ui);
    });
}

extern "C" fn on_au_index(_: Id, _: Sel, sender: Id) {
    let (idx, k) = unsafe {
        let k = selected_tag(sender).unwrap_or(0);
        (tag_of(sender) as usize, k as f32)
    };
    with_ui(|ui| unsafe {
        let (Some(p), Some(a)) = (ui.au_params.get(idx).cloned(), ui.pipe.au()) else {
            return;
        };
        let _ = a.handle().set_param(p.id, p.min.round() + k);
        au_edited(ui);
    });
}

extern "C" fn on_save_preset(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe {
        let Some(name) = ui.cfg.au_preset.clone() else {
            return;
        };
        let r = ui.write_preset(&name);
        log(&format!("save preset {name}: {r:?}"));
        match r {
            Ok(()) => ui.refresh_au_preset(),
            Err(e) => notify_error("Не удалось сохранить пресет", &e),
        }
    });
}

extern "C" fn on_save_preset_as(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe {
        let Some(a) = ui.pipe.au() else { return };
        let dir = preset_dir(&a.name);
        let default = ui
            .cfg
            .au_preset
            .as_ref()
            .map(|n| format!("{n} 2"))
            .unwrap_or_else(|| "Мой пресет".into());
        let Some(name) = prompt(
            "Сохранить пресет плагина",
            &format!(
                "Файл ляжет в {} — его увидят и сам плагин, Logic и GarageBand.",
                dir.display()
            ),
            &default,
            "Сохранить",
        ) else {
            return;
        };

        if dir.join(format!("{name}.aupreset")).exists()
            && !confirm(
                &format!("Заменить \u{201c}{name}\u{201d}?"),
                "Пресет с таким именем уже есть.",
                "Заменить",
            )
        {
            return;
        }

        let r = ui.write_preset(&name);
        log(&format!("save preset as {name}: {r:?}"));
        match r {
            Ok(()) => ui.schedule_rebuild(),
            Err(e) => notify_error("Не удалось сохранить пресет", &e),
        }
    });
}

extern "C" fn on_delete_preset(_: Id, _: Sel, _: Id) {
    with_ui(|ui| unsafe {
        let Some(name) = ui.cfg.au_preset.clone() else {
            return;
        };
        let Some((_, path)) = ui.au_user_presets.iter().find(|p| p.0 == name).cloned() else {
            return;
        };
        if !confirm(
            &format!("Удалить пресет \u{201c}{name}\u{201d}?"),
            "Файл будет перемещён в Корзину.",
            "В Корзину",
        ) {
            return;
        }

        let ok = trash(&path);
        log(&format!("delete preset {name}: {ok}"));
        if ok {
            ui.cfg.au_preset = None;
            ui.save();
            ui.schedule_rebuild();
        } else {
            notify_error("Не удалось удалить пресет", &path.display().to_string());
        }
    });
}

extern "C" fn on_open_plugin_ui(_: Id, _: Sel, _: Id) {
    log("open plugin window");
    with_ui(|ui| unsafe { ui.open_plugin_window() });
}

fn terminate() {
    let ui = UI.with(|cell| cell.try_borrow_mut().ok().and_then(|mut g| g.take()));
    if let Some(mut ui) = ui {
        ui.shutdown();
        drop(ui);
    }
    unsafe { send!(ns_app(), "terminate:", ptr::null_mut::<c_void>() => Id; -> ()) };
}

extern "C" fn on_quit(_: Id, _: Sel, _: Id) {
    terminate();
}

extern "C" fn on_tick(_: Id, _: Sel, _: Id) {
    if TERMINATE.load(Relaxed) {
        terminate();
        return;
    }
    with_ui(|ui| unsafe {
        ui.ticks += 1;
        if ui.pipe.needs_rebuild(&ui.cfg) {
            let had_au = ui.pipe.au().is_some();
            ui.apply();
            if ui.popover_shown() && ui.pipe.au().is_some() != had_au {
                ui.schedule_rebuild();
            }
        } else if ui.dirty {
            ui.save();
        }
        if ui.ticks % 15 == 0 {
            ui.pipe.save_au_state();
        }
        ui.update_icon();
        if ui.popover_shown() {
            ui.refresh_header();
        }
    });
}

extern "C" fn on_signal(_: libc::c_int) {
    TERMINATE.store(true, Relaxed);
}

unsafe fn make_controller() -> Id {
    let cls = objc_allocateClassPair(class(c"NSObject"), c"VinylizerController".as_ptr(), 0);
    let methods: &[(&CStr, extern "C" fn(Id, Sel, Id))] = &[
        (c"togglePopover:", on_toggle_popover),
        (c"popoverDidClose:", on_popover_closed),
        (c"rebuild:", on_rebuild),
        (c"toggleEnabled:", on_toggle_enabled),
        (c"selectTarget:", on_select_target),
        (c"selectMode:", on_select_mode),
        (c"selectEffect:", on_select_effect),
        (c"toggleBypass:", on_toggle_bypass),
        (c"selectPreset:", on_select_preset),
        (c"saveBuiltinPreset:", on_save_builtin_preset),
        (c"saveBuiltinPresetAs:", on_save_builtin_preset_as),
        (c"deleteBuiltinPreset:", on_delete_builtin_preset),
        (c"resetBuiltin:", on_reset_builtin),
        (c"resetPlugin:", on_reset_plugin),
        (c"selectRpm:", on_select_rpm),
        (c"sliderChanged:", on_slider),
        (c"openPluginUI:", on_open_plugin_ui),
        (c"selectAuPreset:", on_select_au_preset),
        (c"auSliderChanged:", on_au_slider),
        (c"auToggle:", on_au_toggle),
        (c"auIndex:", on_au_index),
        (c"savePreset:", on_save_preset),
        (c"savePresetAs:", on_save_preset_as),
        (c"deletePreset:", on_delete_preset),
        (c"quit:", on_quit),
        (c"tick:", on_tick),
    ];
    for (name, f) in methods {
        class_addMethod(cls, sel(name), *f as *const c_void, c"v@:@".as_ptr());
    }
    let proto = objc_getProtocol(c"NSPopoverDelegate".as_ptr());
    if !proto.is_null() {
        class_addProtocol(cls, proto);
    }
    objc_registerClassPair(cls);
    send!(send!(cls, "alloc"; -> Id), "init"; -> Id)
}

/// NSView with a top-left origin, so rows can be laid out downwards.
unsafe fn make_flipped_view_class() -> Id {
    let cls = objc_allocateClassPair(class(c"NSView"), c"VinylizerFlippedView".as_ptr(), 0);
    class_addMethod(cls, sel(c"isFlipped"), is_flipped as *const c_void, BOOL_RET_TYPES.as_ptr());
    objc_registerClassPair(cls);
    cls
}

/// Transient popover: closes when the user clicks anywhere outside it.
unsafe fn make_popover(delegate: Id) -> (Id, Id) {
    let scroll = send!(class(c"NSScrollView"), "alloc"; -> Id);
    let scroll = send!(scroll, "initWithFrame:", NSRect::new(0.0, 0.0, W, 300.0) => NSRect; -> Id);
    send!(scroll, "setHasVerticalScroller:", YES => ObjcBool; -> ());
    send!(scroll, "setAutohidesScrollers:", YES => ObjcBool; -> ());
    send!(scroll, "setDrawsBackground:", NO => ObjcBool; -> ());
    send!(scroll, "setBorderType:", 0usize => usize; -> ());

    let vc = send!(send!(class(c"NSViewController"), "alloc"; -> Id), "init"; -> Id);
    send!(vc, "setView:", scroll => Id; -> ());
    send!(scroll, "release"; -> ());

    let popover = send!(send!(class(c"NSPopover"), "alloc"; -> Id), "init"; -> Id);
    send!(popover, "setBehavior:", 1isize => isize; -> ()); // transient
    send!(popover, "setContentViewController:", vc => Id; -> ());
    send!(vc, "release"; -> ());
    send!(popover, "setDelegate:", delegate => Id; -> ());
    (popover, scroll)
}

/// Dev aid: `VINYLIZER_SNAPSHOT=<dir>` renders the popover content in both modes to
/// `<dir>/builtin.png` and `<dir>/plugin.png`, then exits. Settings are not saved.
unsafe fn snapshot(ui: &mut Ui, dir: &str) {
    for (mode, name) in [(Mode::Builtin, "builtin"), (Mode::Plugin, "plugin")] {
        ui.cfg.mode = mode;
        ui.cfg.enabled = true;
        let _ = ui.pipe.apply(&ui.cfg);

        let h = ui.rebuild();
        let doc = send!(ui.scroll, "documentView"; -> Id);
        // Light appearance, so labels are visible on the transparent PNG.
        let aqua = ns("NSAppearanceNameAqua");
        let appearance = send!(class(c"NSAppearance"), "appearanceNamed:", aqua.0 as Id => Id; -> Id);
        send!(doc, "setAppearance:", appearance => Id; -> ());
        let rect = NSRect::new(0.0, 0.0, W, h);
        let rep = send!(doc, "bitmapImageRepForCachingDisplayInRect:", rect => NSRect; -> Id);
        send!(doc, "cacheDisplayInRect:toBitmapImageRep:", rect => NSRect, rep => Id; -> ());

        let props = send!(class(c"NSDictionary"), "dictionary"; -> Id);
        let png = send!(rep, "representationUsingType:properties:", 4usize => usize, props => Id; -> Id);
        let path = ns(&format!("{dir}/{name}.png"));
        let ok = send!(png, "writeToFile:atomically:", path.0 as Id => Id, YES => ObjcBool; -> ObjcBool);
        eprintln!("snapshot {name}: {} ({h} pt)", ok == YES);
    }
}

pub fn run() {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        // Single instance: two copies would tap each other's output.
        let bid = ns("dev.vinylizer.app");
        let running = send!(class(c"NSRunningApplication"), "runningApplicationsWithBundleIdentifier:", bid.0 as Id => Id; -> Id);
        if !running.is_null() && send!(running, "count"; -> usize) > 1 {
            eprintln!("Vinylizer is already running");
            return;
        }
        let app = ns_app();
        send!(app, "setActivationPolicy:", 1isize => isize; -> ObjcBool); // accessory: no Dock icon

        let controller = make_controller();
        let flipped = make_flipped_view_class();
        let (popover, scroll) = make_popover(controller);
        let params = Arc::new(Params::new());
        let cfg_path = support_dir().join("settings.txt");
        let mut cfg = Settings::load(&cfg_path, &params);
        // The UI offers exactly two modes; chaining stays a CLI option.
        cfg.chain = false;
        let pipe = Pipeline::new(params);

        // Start near the clock (distance from the right edge, in points) unless the user
        // already moved it; ⌘-drag positions are remembered via autosaveName.
        let defaults = send!(class(c"NSUserDefaults"), "standardUserDefaults"; -> Id);
        let key = ns("NSStatusItem Preferred Position Vinylizer");
        let existing = send!(defaults, "objectForKey:", key.0 as Id => Id; -> Id);
        if existing.is_null() {
            send!(defaults, "setDouble:forKey:", 160.0f64 => f64, key.0 as Id => Id; -> ());
        }
        let vis = ns("NSStatusItem Visible Vinylizer");
        send!(defaults, "setBool:forKey:", YES => ObjcBool, vis.0 as Id => Id; -> ());
        let bar = send!(class(c"NSStatusBar"), "systemStatusBar"; -> Id);
        let item = send!(bar, "statusItemWithLength:", -1.0f64 => f64; -> Id);
        send!(item, "retain"; -> Id);
        let autosave = ns("Vinylizer");
        send!(item, "setAutosaveName:", autosave.0 as Id => Id; -> ());
        let button = send!(item, "button"; -> Id);
        let sym = ns("opticaldisc");
        let img = send!(class(c"NSImage"), "imageWithSystemSymbolName:accessibilityDescription:",
            sym.0 as Id => Id, ptr::null_mut::<c_void>() => Id; -> Id);
        if !img.is_null() {
            send!(img, "setTemplate:", YES => ObjcBool; -> ());
            send!(button, "setImage:", img => Id; -> ());
        } else {
            let t = ns("◉");
            send!(button, "setTitle:", t.0 as Id => Id; -> ());
        }
        send!(button, "setTarget:", controller => Id; -> ());
        send!(button, "setAction:", sel(c"togglePopover:") => Sel; -> ());

        log("--- Vinylizer started");
        let mut effects: Vec<String> = au::list_effects().into_iter().map(|c| c.name).collect();
        effects.sort_by_key(|n| (!n.to_lowercase().contains("vinyl"), n.to_lowercase()));
        effects.dedup();
        log(&format!("AU effects: {effects:?}"));

        let mut ui = Ui {
            pipe,
            cfg,
            cfg_path,
            controller,
            flipped,
            status_item: item,
            popover,
            scroll,
            closed_at: None,
            effects,
            apps: Vec::new(),
            c: Controls::default(),
            builtin_user: Vec::new(),
            plugin_window: ptr::null_mut(),
            au_presets: Vec::new(),
            au_user_presets: Vec::new(),
            au_params: Vec::new(),
            au_modified: false,
            dirty: false,
            ticks: 0,
        };
        if let Ok(dir) = std::env::var("VINYLIZER_SNAPSHOT") {
            snapshot(&mut ui, &dir);
            ui.pipe.stop();
            return;
        }
        ui.apply();
        UI.with(|cell| *cell.borrow_mut() = Some(ui));

        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);

        send!(class(c"NSTimer"), "scheduledTimerWithTimeInterval:target:selector:userInfo:repeats:",
            2.0f64 => f64, controller => Id, sel(c"tick:") => Sel, ptr::null_mut::<c_void>() => Id, YES => ObjcBool; -> Id);

        objc_autoreleasePoolPop(pool);
        send!(app, "run"; -> ());
    }
}
