//! Owns the whole audio graph and rebuilds it when the configuration,
//! the output device or the set of target processes changes.
//!
//! The AU instance lives here, outside the per-route graph, so the plugin
//! window and its settings survive route rebuilds (device switch, app restart).

use crate::au::AuPlugin;
use crate::engine::{idle_io_proc, io_proc, Engine, IdleEngine, Meter};
use crate::ffi::kAudioObjectPropertyScopeInput;
use crate::hal::{self, Aggregate, Device, IoProc, Result, Tap, TapTarget};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use vinylizer::i18n::Lang;
use vinylizer::params::Params;
use vinylizer::settings::{au_state_file, Settings};

const AU_MAX_FRAMES: usize = 4096;

struct Route {
    io: Option<IoProc>,
    agg: Option<Aggregate>,
    tap: Option<Tap>,
    engine: *mut Engine,
    out: Device,
    objs: Vec<u32>,
    sample_rate: f64,
    buffer: u32,
}

impl Drop for Route {
    fn drop(&mut self) {
        // Order matters: stop + destroy the IOProc before freeing what it references.
        drop(self.io.take());
        drop(self.agg.take());
        drop(self.tap.take());
        if !self.engine.is_null() {
            drop(unsafe { Box::from_raw(self.engine) });
        }
    }
}

/// The needle playing on its own on the output device (see `IdleEngine`).
struct IdleRoute {
    io: Option<IoProc>,
    engine: *mut IdleEngine,
}

impl Drop for IdleRoute {
    fn drop(&mut self) {
        // Stop the IOProc before freeing the engine it points to.
        drop(self.io.take());
        if !self.engine.is_null() {
            drop(unsafe { Box::from_raw(self.engine) });
        }
    }
}

impl IdleRoute {
    fn start(params: Arc<Params>, meter: Arc<Meter>, out: &Device) -> Result<IdleRoute> {
        let sample_rate = hal::sample_rate(out.id)?;
        let engine = Box::into_raw(Box::new(IdleEngine::new(params, meter, sample_rate)));
        let mut route = IdleRoute { io: None, engine };

        let mut io = IoProc::new(out.id, idle_io_proc, engine as *mut _)?;
        // Output only: an open microphone would drop a Bluetooth speaker into HFP.
        let inputs = hal::stream_count(out.id, kAudioObjectPropertyScopeInput);
        if inputs > 0 {
            let _ = io.restrict_input_streams(inputs, 0);
        }
        io.start()?;
        route.io = Some(io);
        Ok(route)
    }
}

/// What the pipeline is doing, for the status line; `text` renders it in a language.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Off,
    /// Selected apps are not running yet.
    Waiting,
    Error(String),
    Running {
        effect: String,
        device: String,
        sample_rate: f64,
        buffer: u32,
    },
}

impl Status {
    pub fn text(&self, lang: Lang) -> String {
        match self {
            Status::Off => lang.pick("Выключено", "Off").into(),
            Status::Waiting => lang
                .pick(
                    "Ждём, пока выбранные приложения заиграют…",
                    "Waiting for the selected apps to play…",
                )
                .into(),
            Status::Error(e) => format!("{}: {e}", lang.pick("Ошибка", "Error")),
            Status::Running {
                effect,
                device,
                sample_rate,
                buffer,
            } => format!(
                "{effect} → {device} · {:.0} kHz · {buffer} fr",
                sample_rate / 1000.0
            ),
        }
    }
}

pub struct Pipeline {
    pub params: Arc<Params>,
    pub meter: Arc<Meter>,
    au: Option<Box<AuPlugin>>,
    route: Option<Route>,
    idle: Option<IdleRoute>,
    pub status: Status,
}

/// Processes whose bundle id starts with one of the targets.
fn matching_objects(targets: &[String]) -> Result<Vec<u32>> {
    let mut v: Vec<u32> = hal::list_processes()?
        .into_iter()
        .filter(|p| targets.iter().any(|t| p.bundle.starts_with(t.as_str())))
        .map(|p| p.obj)
        .collect();
    v.sort_unstable();
    Ok(v)
}

impl Pipeline {
    pub fn new(params: Arc<Params>) -> Self {
        Self {
            params,
            meter: Arc::new(Meter::default()),
            au: None,
            route: None,
            idle: None,
            status: Status::Off,
        }
    }

    pub fn au(&self) -> Option<&AuPlugin> {
        self.au.as_deref()
    }

    pub fn is_running(&self) -> bool {
        self.route.is_some()
    }

    pub fn stop(&mut self) {
        self.route = None;
        self.idle = None;
    }

    /// Start the needle-on-its-own path when it is wanted: built-in effect, and
    /// "crackle without music" on. A third-party plugin cannot run in two places at once.
    fn start_idle(&mut self, cfg: &Settings, out: &Device) {
        if cfg.active_au().is_some() || !self.params.idle_surface.load(Relaxed) {
            return;
        }

        match IdleRoute::start(self.params.clone(), self.meter.clone(), out) {
            Ok(r) => self.idle = Some(r),
            Err(e) => vinylizer::settings::log(&format!("idle needle failed: {e}")),
        }
    }

    /// Persist the current plugin's state next to the settings.
    pub fn save_au_state(&self) {
        if let Some(au) = &self.au {
            if let Some(bytes) = au.handle().save_state() {
                let path = au_state_file(&au.name);
                let _ = std::fs::create_dir_all(path.parent().unwrap());
                let _ = std::fs::write(path, bytes);
            }
        }
    }

    /// Make the loaded AU match `want` (opening, swapping or closing it).
    /// Requires the route to be stopped.
    fn sync_au(&mut self, want: Option<&str>, sample_rate: f64) -> Result<()> {
        debug_assert!(self.route.is_none());
        match (want, self.au.as_ref()) {
            (None, None) => return Ok(()),
            (Some(w), Some(a)) if a.name == w => {
                return self.au.as_mut().unwrap().reconfigure(sample_rate)
            }
            _ => {}
        }
        self.save_au_state();
        self.au = None;
        if let Some(name) = want {
            let au = AuPlugin::open(name, sample_rate, AU_MAX_FRAMES)?;
            if let Ok(bytes) = std::fs::read(au_state_file(&au.name)) {
                let _ = au.handle().load_state(&bytes);
            }
            self.au = Some(Box::new(au));
        }
        Ok(())
    }

    /// Tear down and rebuild the graph for `cfg`.
    pub fn apply(&mut self, cfg: &Settings) -> Result<()> {
        self.stop();
        let r = self.apply_inner(cfg);
        if let Err(e) = &r {
            self.status = Status::Error(e.to_string());
        }
        r
    }

    fn apply_inner(&mut self, cfg: &Settings) -> Result<()> {
        if !cfg.enabled {
            self.status = Status::Off;
            return Ok(());
        }
        let out = hal::default_output()?;

        let (target, objs) = if cfg.targets.is_empty() {
            let me = hal::process_object_for_pid(std::process::id() as i32)
                .ok_or("cannot resolve own audio process (needed to avoid feedback)")?;
            (TapTarget::Global { exclude: vec![me] }, Vec::new())
        } else {
            let objs = matching_objects(&cfg.targets)?;
            if objs.is_empty() {
                // AU may still be (re)opened so its window is usable while waiting.
                self.sync_au(
                    cfg.active_au(),
                    hal::sample_rate(out.id).unwrap_or(48_000.0),
                )?;
                self.status = Status::Waiting;
                self.start_idle(cfg, &out);
                return Ok(());
            }
            (TapTarget::Processes(objs.clone()), objs)
        };

        let tap = Tap::create(&target)?;
        let agg = Aggregate::create(&out.uid, &tap.uuid)?;
        let sample_rate = hal::sample_rate(agg.id)?;
        let buffer = hal::buffer_frames(agg.id).unwrap_or(512);

        self.sync_au(cfg.active_au(), sample_rate)?;
        let au_ptr: *mut AuPlugin = self
            .au
            .as_deref_mut()
            .map_or(std::ptr::null_mut(), |a| a as *mut AuPlugin);
        let engine = Box::into_raw(Box::new(Engine::new(
            self.params.clone(),
            self.meter.clone(),
            sample_rate,
            au_ptr,
            cfg.chain,
        )));

        let mut route = Route {
            io: None,
            agg: Some(agg),
            tap: Some(tap),
            engine,
            out,
            objs,
            sample_rate,
            buffer,
        };
        let agg_id = route.agg.as_ref().unwrap().id;
        let mut io = IoProc::new(agg_id, io_proc, engine as *mut _)?;
        let total_in = hal::stream_count(agg_id, kAudioObjectPropertyScopeInput);
        let dev_in = hal::stream_count(route.out.id, kAudioObjectPropertyScopeInput);
        if dev_in > 0 && total_in > dev_in {
            let _ = io.restrict_input_streams(total_in, total_in - dev_in);
        }
        io.start()?;
        route.io = Some(io);
        let out = route.out.clone();
        self.route = Some(route);
        self.start_idle(cfg, &out);

        let route = self.route.as_ref().unwrap();
        let effect = match &self.au {
            Some(a) if cfg.chain => format!("{} + Vinylizer", a.name),
            Some(a) => a.name.clone(),
            None => "Vinylizer".into(),
        };
        self.status = Status::Running {
            effect,
            device: route.out.name.clone(),
            sample_rate: route.sample_rate,
            buffer: route.buffer,
        };
        Ok(())
    }

    /// Cheap periodic check: did the output device or target process set change?
    pub fn needs_rebuild(&self, cfg: &Settings) -> bool {
        if !cfg.enabled {
            return self.route.is_some();
        }
        let out = hal::default_output().map(|d| d.id).unwrap_or(0);
        match &self.route {
            None => {
                // waiting for apps (or a previous error): retry when something shows up
                !cfg.targets.is_empty()
                    && matching_objects(&cfg.targets)
                        .map(|o| !o.is_empty())
                        .unwrap_or(false)
            }
            Some(r) => {
                r.out.id != out
                    || (!cfg.targets.is_empty()
                        && matching_objects(&cfg.targets)
                            .map(|o| o != r.objs)
                            .unwrap_or(false))
            }
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
        self.save_au_state();
    }
}
