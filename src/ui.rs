#![allow(non_upper_case_globals)]
//! Menu bar app (AppKit through the raw Objective-C runtime — no extra crates).
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
use vinylizer::params::{Params, PARAM_NAMES, PRESETS};
use vinylizer::settings::{log, support_dir, Settings};

#[cfg(target_arch = "aarch64")]
const NO: ObjcBool = false;
#[cfg(not(target_arch = "aarch64"))]
const NO: ObjcBool = 0;

const OWN_BUNDLE: &str = "dev.vinylizer";
static TERMINATE: AtomicBool = AtomicBool::new(false);

struct AppEntry {
    root: String,
    name: String,
    playing: bool,
    running: bool,
}

struct Ui {
    pipe: Pipeline,
    cfg: Settings,
    cfg_path: PathBuf,
    controller: Id,
    status_item: Id,
    effects: Vec<String>,
    apps: Vec<AppEntry>,
    value_labels: Vec<Id>,
    plugin_window: Id,
    au_presets: Vec<(i32, String)>,
    au_user_presets: Vec<(String, PathBuf)>,
    au_params: Vec<au::AuParam>,
    au_value_labels: Vec<Id>,
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

unsafe fn new_menu(title: &str) -> Id {
    let t = ns(title);
    let m =
        send!(send!(class(c"NSMenu"), "alloc"; -> Id), "initWithTitle:", t.0 as Id => Id; -> Id);
    send!(m, "setAutoenablesItems:", NO => ObjcBool; -> ());
    send!(m, "autorelease"; -> Id)
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

unsafe fn label(text: &str, frame: NSRect, size: f64) -> Id {
    let t = ns(text);
    let l = send!(class(c"NSTextField"), "labelWithString:", t.0 as Id => Id; -> Id);
    send!(l, "setFrame:", frame => NSRect; -> ());
    let font = send!(class(c"NSFont"), "systemFontOfSize:", size => f64; -> Id);
    send!(l, "setFont:", font => Id; -> ());
    l
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
    let alert = new_alert(title, info, &[ok, "Cancel"]);
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
    let alert = new_alert(title, info, &[ok, "Cancel"]);
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
    let (manu, plug) = component_name
        .split_once(':')
        .map(|(m, p)| (m.trim(), p.trim()))
        .unwrap_or(("", component_name));
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

// ------------------------------------------------------------------ Ui logic

impl Ui {
    fn save(&mut self) {
        let _ = self.cfg.save(&self.cfg_path, &self.pipe.params);
        self.dirty = false;
    }

    fn apply(&mut self) {
        let current = self.pipe.au().map(|a| a.name.clone());
        if current != self.cfg.au || !self.cfg.enabled {
            // plugin instance may be replaced: its editor must go first
            if current != self.cfg.au {
                unsafe { self.close_plugin_window() };
            }
        }
        let r = self.pipe.apply(&self.cfg);
        log(&format!(
            "apply enabled={} targets={:?} au={:?} chain={} -> {:?} | {} | au_loaded={:?}",
            self.cfg.enabled,
            self.cfg.targets,
            self.cfg.au,
            self.cfg.chain,
            r.as_ref().map(|_| ()),
            self.pipe.status,
            self.pipe.au().map(|a| a.name.clone())
        ));
        if let Some(a) = self.pipe.au() {
            log(&format!(
                "AU factory presets: {:?}",
                a.handle().factory_presets()
            ));
        }
        self.save();
        unsafe { self.update_icon() };
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

    unsafe fn populate(&mut self, menu: Id) {
        send!(menu, "removeAllItems"; -> ());
        let c = self.controller;

        let st = add(menu, menu_item(&self.pipe.status, None, c, 0));
        set_enabled(st, false);
        let en = add(menu, menu_item("Enabled", Some(c"toggleEnabled:"), c, 0));
        set_state(en, self.cfg.enabled);
        separator(menu);

        // ---- which apps
        self.apps = list_apps(&self.cfg.targets);
        let chosen: Vec<&str> = self
            .apps
            .iter()
            .filter(|a| self.cfg.targets.contains(&a.root))
            .map(|a| a.name.as_str())
            .collect();
        let title = if self.cfg.targets.is_empty() {
            "Apply to: All Apps".to_string()
        } else {
            format!("Apply to: {}", chosen.join(", "))
        };
        let apply = add(menu, menu_item(&title, None, c, 0));
        let sub = new_menu("Apply to");
        set_state(
            add(sub, menu_item("All Apps", Some(c"selectTarget:"), c, -1)),
            self.cfg.targets.is_empty(),
        );
        separator(sub);
        if self.apps.is_empty() {
            set_enabled(
                add(sub, menu_item("No apps using audio", None, c, 0)),
                false,
            );
        }
        for (i, a) in self.apps.iter().enumerate() {
            let t = match (a.playing, a.running) {
                (true, _) => format!("{}   ♪", a.name),
                (false, false) => format!("{}   (not running)", a.name),
                _ => a.name.clone(),
            };
            set_state(
                add(sub, menu_item(&t, Some(c"selectTarget:"), c, i as isize)),
                self.cfg.targets.contains(&a.root),
            );
        }
        send!(apply, "setSubmenu:", sub => Id; -> ());

        // ---- effect
        let cur = self
            .cfg
            .au
            .clone()
            .unwrap_or_else(|| "Built-in Vinyl".into());
        let eff = add(menu, menu_item(&format!("Effect: {cur}"), None, c, 0));
        let sub = new_menu("Effect");
        for (i, n) in self.effects.iter().enumerate() {
            set_state(
                add(sub, menu_item(n, Some(c"selectEffect:"), c, i as isize)),
                self.cfg.au.as_deref() == Some(n),
            );
        }
        separator(sub);
        set_state(
            add(
                sub,
                menu_item("Built-in Vinyl", Some(c"selectEffect:"), c, -1),
            ),
            self.cfg.au.is_none(),
        );
        send!(eff, "setSubmenu:", sub => Id; -> ());
        separator(menu);

        if self.cfg.au.is_some() {
            self.populate_au(menu);
        } else {
            self.populate_builtin(menu);
        }
        separator(menu);
        add(menu, menu_item("Quit Vinylizer", Some(c"quit:"), c, 0));
    }

    unsafe fn populate_au(&mut self, menu: Id) {
        let c = self.controller;
        let Some(plugin) = self.pipe.au() else {
            set_enabled(
                add(
                    menu,
                    menu_item("Plugin not loaded (enable Vinylizer)", None, c, 0),
                ),
                false,
            );
            return;
        };
        let h = plugin.handle();
        let plugin_name = plugin.name.clone();

        // ---- presets
        self.au_user_presets = user_presets(&plugin_name);
        self.au_presets = h.factory_presets();
        let only_default = self.au_presets.len() == 1;
        self.au_presets.retain(|p| p.1 != "Default" || only_default);
        let cur = self.cfg.au_preset.clone();
        let cur_label = match (&cur, self.au_modified) {
            (Some(n), true) => format!("Preset: {n} (edited)"),
            (Some(n), false) => format!("Preset: {n}"),
            (None, _) => "Preset: —".into(),
        };
        let pp = add(menu, menu_item(&cur_label, None, c, 0));
        let sub = new_menu("Preset");
        for (i, (name, _)) in self.au_user_presets.iter().enumerate() {
            set_state(
                add(
                    sub,
                    menu_item(name, Some(c"selectAuUserPreset:"), c, i as isize),
                ),
                cur.as_deref() == Some(name),
            );
        }
        if !self.au_user_presets.is_empty() && !self.au_presets.is_empty() {
            separator(sub);
        }
        for (num, name) in &self.au_presets {
            set_state(
                add(
                    sub,
                    menu_item(name, Some(c"selectAuPreset:"), c, *num as isize),
                ),
                cur.as_deref() == Some(name),
            );
        }
        send!(pp, "setSubmenu:", sub => Id; -> ());

        let is_user = cur
            .as_ref()
            .is_some_and(|n| self.au_user_presets.iter().any(|p| &p.0 == n));
        let save_title = match (&cur, is_user) {
            (Some(n), true) => format!("Save Preset \u{201c}{n}\u{201d}"),
            _ => "Save Preset".into(),
        };
        let save = add(menu, menu_item(&save_title, Some(c"savePreset:"), c, 0));
        set_enabled(save, is_user && self.au_modified);
        add(
            menu,
            menu_item("Save Preset As…", Some(c"savePresetAs:"), c, 0),
        );
        let del = add(
            menu,
            menu_item("Delete Preset…", Some(c"deletePreset:"), c, 0),
        );
        set_enabled(del, is_user);
        add(
            menu,
            menu_item("Open Plugin Window…", Some(c"openPluginUI:"), c, 0),
        );
        separator(menu);

        // ---- live parameters
        self.au_params = h
            .params()
            .into_iter()
            .filter(|p| p.flags & kAudioUnitParameterFlag_IsWritable != 0 && p.max > p.min)
            .collect();
        self.au_value_labels = vec![ptr::null_mut(); self.au_params.len()];
        let hdr = add(menu, menu_item(&plugin_name, None, c, 0));
        set_enabled(hdr, false);
        for i in 0..self.au_params.len() {
            let p = self.au_params[i].clone();
            let v = h.get_param(p.id);
            match p.unit {
                kAudioUnitParameterUnit_Boolean => {
                    let it = add(menu, menu_item(&p.name, Some(c"auToggle:"), c, i as isize));
                    set_state(it, v >= 0.5);
                }
                kAudioUnitParameterUnit_Indexed => {
                    let names = h.value_strings(p.id);
                    let idx = v.round() as i64;
                    let shown = names
                        .get((idx - p.min as i64).max(0) as usize)
                        .cloned()
                        .unwrap_or_else(|| format!("{idx}"));
                    let it = add(menu, menu_item(&format!("{}: {shown}", p.name), None, c, 0));
                    let sub = new_menu(&p.name);
                    let lo = p.min.round() as i64;
                    let hi = p.max.round() as i64;
                    for k in lo..=hi {
                        let label = names
                            .get((k - lo) as usize)
                            .cloned()
                            .unwrap_or_else(|| format!("{k}"));
                        let tag = ((i as isize) << 16) | ((k - lo) as isize);
                        set_state(
                            add(sub, menu_item(&label, Some(c"auIndex:"), c, tag)),
                            k == idx,
                        );
                    }
                    send!(it, "setSubmenu:", sub => Id; -> ());
                }
                _ => {
                    let lbl = self.param_slider(
                        menu,
                        i,
                        &p.name,
                        v as f64,
                        p.min as f64,
                        p.max as f64,
                        &p.format(v),
                        c"auSliderChanged:",
                    );
                    self.au_value_labels[i] = lbl;
                }
            }
        }
        if self.au_params.is_empty() {
            set_enabled(
                add(menu, menu_item("No editable parameters", None, c, 0)),
                false,
            );
        }
    }

    unsafe fn populate_builtin(&mut self, menu: Id) {
        let c = self.controller;
        let pr = add(menu, menu_item("Preset", None, c, 0));
        let sub = new_menu("Preset");
        for (i, (n, _)) in PRESETS.iter().enumerate() {
            add(sub, menu_item(n, Some(c"selectPreset:"), c, i as isize));
        }
        send!(pr, "setSubmenu:", sub => Id; -> ());
        let bp = add(menu, menu_item("Bypass", Some(c"toggleBypass:"), c, 0));
        set_state(bp, self.pipe.params.bypass.load(Relaxed));
        self.value_labels = vec![ptr::null_mut(); PARAM_NAMES.len()];
        for (i, n) in PARAM_NAMES.iter().enumerate() {
            let v = self.pipe.params.get(n).unwrap_or(0.0) as f64;
            let max = if *n == "gain" { 2.0 } else { 1.0 };
            self.value_labels[i] = self.param_slider(
                menu,
                i,
                n,
                v,
                0.0,
                max,
                &format!("{v:.2}"),
                c"sliderChanged:",
            );
        }
    }

    /// Menu row: name | slider | value. Returns the value label.
    #[allow(clippy::too_many_arguments)]
    unsafe fn param_slider(
        &mut self,
        menu: Id,
        tag: usize,
        name: &str,
        value: f64,
        min: f64,
        max: f64,
        text: &str,
        action: &CStr,
    ) -> Id {
        let view = send!(send!(class(c"NSView"), "alloc"; -> Id), "initWithFrame:", NSRect::new(0.0, 0.0, 380.0, 26.0) => NSRect; -> Id);
        let name_l = label(name, NSRect::new(20.0, 4.0, 140.0, 17.0), 12.0);
        let val_l = label(text, NSRect::new(316.0, 4.0, 62.0, 17.0), 11.0);
        let slider = send!(class(c"NSSlider"), "sliderWithValue:minValue:maxValue:target:action:",
            value => f64, min => f64, max => f64, self.controller => Id, sel(action) => Sel; -> Id);
        send!(slider, "setFrame:", NSRect::new(160.0, 3.0, 152.0, 20.0) => NSRect; -> ());
        send!(slider, "setTag:", tag as isize => isize; -> ());
        send!(slider, "setContinuous:", YES => ObjcBool; -> ());
        send!(slider, "setControlSize:", 1isize => isize; -> ());
        send!(view, "addSubview:", name_l => Id; -> ());
        send!(view, "addSubview:", slider => Id; -> ());
        send!(view, "addSubview:", val_l => Id; -> ());
        let item = menu_item(name, None, self.controller, 0);
        send!(item, "setView:", view => Id; -> ());
        send!(view, "release"; -> ());
        add(menu, item);
        val_l
    }

    /// Write the plugin's current state as `<name>.aupreset` in the user preset folder.
    fn write_preset(&mut self, name: &str) -> Result<(), String> {
        let a = self.pipe.au().ok_or("no plugin loaded")?;
        let bytes = a
            .handle()
            .export_preset(name)
            .ok_or("plugin did not return its state")?;
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

extern "C" fn on_toggle_enabled(_: Id, _: Sel, _: Id) {
    with_ui(|ui| {
        ui.cfg.enabled = !ui.cfg.enabled;
        ui.apply();
    });
}

extern "C" fn on_select_target(_: Id, _: Sel, sender: Id) {
    let tag = unsafe { tag_of(sender) };
    with_ui(|ui| {
        if tag < 0 {
            ui.cfg.targets.clear();
        } else if let Some(a) = ui.apps.get(tag as usize) {
            if let Some(pos) = ui.cfg.targets.iter().position(|t| t == &a.root) {
                ui.cfg.targets.remove(pos);
            } else {
                ui.cfg.targets.push(a.root.clone());
            }
        }
        ui.apply();
    });
}

extern "C" fn on_select_effect(_: Id, _: Sel, sender: Id) {
    let tag = unsafe { tag_of(sender) };
    with_ui(|ui| {
        ui.cfg.au = if tag < 0 {
            None
        } else {
            ui.effects.get(tag as usize).cloned()
        };
        ui.apply();
    });
}

extern "C" fn on_toggle_chain(_: Id, _: Sel, _: Id) {
    with_ui(|ui| {
        ui.cfg.chain = !ui.cfg.chain;
        ui.apply();
    });
}

extern "C" fn on_toggle_bypass(_: Id, _: Sel, _: Id) {
    with_ui(|ui| {
        let b = !ui.pipe.params.bypass.load(Relaxed);
        ui.pipe.params.bypass.store(b, Relaxed);
    });
}

extern "C" fn on_select_preset(_: Id, _: Sel, sender: Id) {
    let tag = unsafe { tag_of(sender) };
    with_ui(|ui| {
        if let Some((n, _)) = PRESETS.get(tag as usize) {
            let _ = ui.pipe.params.apply_preset(n);
            ui.save();
        }
    });
}

extern "C" fn on_slider(_: Id, _: Sel, sender: Id) {
    unsafe {
        let idx = tag_of(sender) as usize;
        let v = send!(sender, "doubleValue"; -> f64) as f32;
        with_ui(|ui| {
            if let Some(n) = PARAM_NAMES.get(idx) {
                let _ = ui.pipe.params.set(n, v);
                if let Some(&l) = ui.value_labels.get(idx) {
                    if !l.is_null() {
                        let t = ns(&format!("{v:.2}"));
                        send!(l, "setStringValue:", t.0 as Id => Id; -> ());
                    }
                }
                ui.dirty = true;
            }
        });
    }
}

extern "C" fn on_select_au_preset(_: Id, _: Sel, sender: Id) {
    let num = unsafe { tag_of(sender) } as i32;
    with_ui(|ui| {
        if let Some(a) = ui.pipe.au() {
            let r = a.handle().set_preset(num);
            log(&format!("set AU preset #{num}: {r:?}"));
            if r.is_ok() {
                ui.cfg.au_preset = ui
                    .au_presets
                    .iter()
                    .find(|p| p.0 == num)
                    .map(|p| p.1.clone());
                ui.au_modified = false;
                ui.save();
                ui.pipe.save_au_state();
            }
        }
    });
}

extern "C" fn on_select_au_user_preset(_: Id, _: Sel, sender: Id) {
    let idx = unsafe { tag_of(sender) } as usize;
    with_ui(|ui| {
        let Some((name, path)) = ui.au_user_presets.get(idx).cloned() else {
            return;
        };
        let Some(a) = ui.pipe.au() else { return };
        let r = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| a.handle().load_state(&b));
        log(&format!("load user preset {name}: {r:?}"));
        if r.is_ok() {
            ui.cfg.au_preset = Some(name);
            ui.au_modified = false;
            ui.save();
            ui.pipe.save_au_state();
        }
    });
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
            if let Some(&l) = ui.au_value_labels.get(idx) {
                if !l.is_null() {
                    let t = ns(&p.format(v));
                    send!(l, "setStringValue:", t.0 as Id => Id; -> ());
                }
            }
            ui.au_modified = true;
        });
    }
}

extern "C" fn on_au_toggle(_: Id, _: Sel, sender: Id) {
    let idx = unsafe { tag_of(sender) } as usize;
    with_ui(|ui| {
        let (Some(p), Some(a)) = (ui.au_params.get(idx).cloned(), ui.pipe.au()) else {
            return;
        };
        let h = a.handle();
        let v = if h.get_param(p.id) >= 0.5 {
            p.min
        } else {
            p.max
        };
        let _ = h.set_param(p.id, v);
        ui.au_modified = true;
    });
}

extern "C" fn on_au_index(_: Id, _: Sel, sender: Id) {
    let tag = unsafe { tag_of(sender) };
    let (idx, k) = ((tag >> 16) as usize, (tag & 0xffff) as f32);
    with_ui(|ui| {
        let (Some(p), Some(a)) = (ui.au_params.get(idx).cloned(), ui.pipe.au()) else {
            return;
        };
        let _ = a.handle().set_param(p.id, p.min.round() + k);
        ui.au_modified = true;
    });
}

extern "C" fn on_save_preset(_: Id, _: Sel, _: Id) {
    with_ui(|ui| {
        let Some(name) = ui.cfg.au_preset.clone() else {
            return;
        };
        let r = ui.write_preset(&name);
        log(&format!("save preset {name}: {r:?}"));
        if let Err(e) = r {
            unsafe { notify_error("Could not save preset", &e) };
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
            .unwrap_or_else(|| "My Preset".into());
        let Some(name) = prompt(
            "Save Plugin Preset",
            &format!(
                "Saved to {} — also visible in the plugin, Logic and GarageBand.",
                dir.display()
            ),
            &default,
            "Save",
        ) else {
            return;
        };
        if dir.join(format!("{name}.aupreset")).exists()
            && !confirm(
                &format!("Replace \u{201c}{name}\u{201d}?"),
                "A preset with this name already exists.",
                "Replace",
            )
        {
            return;
        }
        let r = ui.write_preset(&name);
        log(&format!("save preset as {name}: {r:?}"));
        if let Err(e) = r {
            notify_error("Could not save preset", &e);
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
            &format!("Delete preset \u{201c}{name}\u{201d}?"),
            "The file will be moved to the Trash.",
            "Move to Trash",
        ) {
            return;
        }
        let ok = trash(&path);
        log(&format!("delete preset {name}: {ok}"));
        if ok {
            ui.cfg.au_preset = None;
            ui.save();
        } else {
            notify_error("Could not delete preset", &path.display().to_string());
        }
    });
}

extern "C" fn on_open_plugin_ui(_: Id, _: Sel, _: Id) {
    log("menu: Open Plugin Window");
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

extern "C" fn on_menu_needs_update(_: Id, _: Sel, menu: Id) {
    with_ui(|ui| unsafe { ui.populate(menu) });
}

extern "C" fn on_tick(_: Id, _: Sel, _: Id) {
    if TERMINATE.load(Relaxed) {
        terminate();
        return;
    }
    with_ui(|ui| {
        ui.ticks += 1;
        if ui.pipe.needs_rebuild(&ui.cfg) {
            ui.apply();
        } else if ui.dirty {
            ui.save();
        }
        if ui.ticks % 15 == 0 {
            ui.pipe.save_au_state();
        }
        unsafe { ui.update_icon() };
    });
}

extern "C" fn on_signal(_: libc::c_int) {
    TERMINATE.store(true, Relaxed);
}

unsafe fn make_controller() -> Id {
    let cls = objc_allocateClassPair(class(c"NSObject"), c"VinylizerController".as_ptr(), 0);
    let methods: [(&CStr, extern "C" fn(Id, Sel, Id)); 19] = [
        (c"toggleEnabled:", on_toggle_enabled),
        (c"selectTarget:", on_select_target),
        (c"selectEffect:", on_select_effect),
        (c"toggleChain:", on_toggle_chain),
        (c"toggleBypass:", on_toggle_bypass),
        (c"selectPreset:", on_select_preset),
        (c"sliderChanged:", on_slider),
        (c"openPluginUI:", on_open_plugin_ui),
        (c"selectAuPreset:", on_select_au_preset),
        (c"selectAuUserPreset:", on_select_au_user_preset),
        (c"auSliderChanged:", on_au_slider),
        (c"auToggle:", on_au_toggle),
        (c"auIndex:", on_au_index),
        (c"savePreset:", on_save_preset),
        (c"savePresetAs:", on_save_preset_as),
        (c"deletePreset:", on_delete_preset),
        (c"quit:", on_quit),
        (c"menuNeedsUpdate:", on_menu_needs_update),
        (c"tick:", on_tick),
    ];
    for (name, f) in methods {
        class_addMethod(cls, sel(name), f as *const c_void, c"v@:@".as_ptr());
    }
    let proto = objc_getProtocol(c"NSMenuDelegate".as_ptr());
    if !proto.is_null() {
        class_addProtocol(cls, proto);
    }
    objc_registerClassPair(cls);
    send!(send!(cls, "alloc"; -> Id), "init"; -> Id)
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
        let params = Arc::new(Params::new());
        let cfg_path = support_dir().join("settings.txt");
        let cfg = Settings::load(&cfg_path, &params);
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
        let menu = new_menu("Vinylizer");
        send!(menu, "setDelegate:", controller => Id; -> ());
        send!(item, "setMenu:", menu => Id; -> ());

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
            status_item: item,
            effects,
            apps: Vec::new(),
            value_labels: Vec::new(),
            plugin_window: ptr::null_mut(),
            au_presets: Vec::new(),
            au_user_presets: Vec::new(),
            au_params: Vec::new(),
            au_value_labels: Vec::new(),
            au_modified: false,
            dirty: false,
            ticks: 0,
        };
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
