//! Real-time engine driven by the aggregate device's IOProc.
//!
//! Hot path guarantees: no allocation, no locks, no syscalls, no logging.
//! Built-in path is single-pass: tap buffer -> DSP -> device buffer.
//! AU path adds one deinterleave + one interleave (AU v2 wants planar audio).

use crate::au::AuPlugin;
use crate::ffi::*;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use vinylizer::dsp::{soft_clip, SilenceGate, Vinyl};
use vinylizer::params::{AtomicF32, Params};

#[derive(Default)]
pub struct Meter {
    pub in_peak: AtomicF32,
    pub out_peak: AtomicF32,
    pub callbacks: AtomicU64,
    pub frames: AtomicU64,
    /// Frames the idle needle played (with any gain); should stay 0 while the main engine runs.
    pub idle_frames: AtomicU64,
}

pub struct Engine {
    params: Arc<Params>,
    meter: Arc<Meter>,
    vinyl: Vinyl,
    /// Silences what the effect adds (crackle, noise, a plugin's own noise) while nothing plays.
    gate: SilenceGate,
    /// Owned by the Pipeline, which outlives every Engine that points to it
    /// and never touches it while an IOProc is running.
    au: *mut AuPlugin,
    chain_builtin: bool,
}

#[derive(Clone, Copy)]
struct Dst {
    ptr: *mut f32,
    stride: usize,
    frames: usize,
}

impl Engine {
    pub fn new(
        params: Arc<Params>,
        meter: Arc<Meter>,
        sample_rate: f64,
        au: *mut AuPlugin,
        chain_builtin: bool,
    ) -> Self {
        let mut vinyl = Vinyl::new(sample_rate);
        vinyl.update(&params);
        Self {
            params,
            meter,
            vinyl,
            gate: SilenceGate::new(sample_rate),
            au,
            chain_builtin,
        }
    }

    #[inline(always)]
    unsafe fn render(&mut self, input: *const AudioBufferList, output: *mut AudioBufferList) {
        let Some((dst, mono_out)) = silenced_stereo_out(output) else {
            return;
        };

        // The tap is the last active input stream of the aggregate.
        let Some(src) = abl_slice(input)
            .iter()
            .rev()
            .find(|b| !b.mData.is_null() && b.mNumberChannels > 0)
        else {
            return;
        };
        let ich = src.mNumberChannels as usize;
        let sp = src.mData as *const f32;
        let frames = (src.mDataByteSize as usize / 4 / ich)
            .min(dst[0].frames)
            .min(dst[1].frames);

        let gain = self.params.gain.get();
        let bypass = self.params.bypass.load(Relaxed);
        let idle_surface = self.params.idle_surface.load(Relaxed);
        self.vinyl.update(&self.params);

        let (d0, d1) = (dst[0], dst[1]);
        let mut in_peak = 0f32;
        let mut out_peak = 0f32;
        let mut write = |i: usize, l: f32, r: f32| {
            let (l, r) = if bypass {
                (l, r)
            } else {
                (soft_clip(l * gain), soft_clip(r * gain))
            };
            out_peak = out_peak.max(l.abs()).max(r.abs());
            if mono_out {
                *d0.ptr.add(i * d0.stride) = 0.5 * (l + r);
            } else {
                *d0.ptr.add(i * d0.stride) = l;
                *d1.ptr.add(i * d1.stride) = r;
            }
        };
        let read = |i: usize| -> (f32, f32) {
            let l = *sp.add(i * ich);
            (l, if ich > 1 { *sp.add(i * ich + 1) } else { l })
        };

        if bypass {
            for i in 0..frames {
                let (l, r) = read(i);
                in_peak = in_peak.max(l.abs()).max(r.abs());
                write(i, l, r);
            }
        } else if let Some(au) = self.au.as_mut() {
            // SAFETY: see field docs.
            let mut done = 0;
            while done < frames {
                let n = (frames - done).min(au.max_frames);
                au.load_input(sp.add(done * ich), ich, n);
                let ok = au.render(n);
                for i in 0..n {
                    let (dl, dr) = au.dry(i);
                    in_peak = in_peak.max(dl.abs()).max(dr.abs());
                    let (mut l, mut r) = if ok { au.out(i) } else { (dl, dr) };

                    // While nothing plays, the plugin's own noise gives way to the (silent) input.
                    let g = self.gate.tick(dl, dr);
                    let g = if idle_surface { 1.0 } else { g };
                    l = l * g + dl * (1.0 - g);
                    r = r * g + dr * (1.0 - g);
                    if self.chain_builtin {
                        self.vinyl.set_surface(g);
                        (l, r) = self.vinyl.tick(l, r);
                    }
                    write(done + i, l, r);
                }
                done += n;
            }
        } else {
            for i in 0..frames {
                let (l, r) = read(i);
                in_peak = in_peak.max(l.abs()).max(r.abs());
                let g = self.gate.tick(l, r);
                self.vinyl.set_surface(if idle_surface { 1.0 } else { g });
                let (l, r) = self.vinyl.tick(l, r);
                write(i, l, r);
            }
        }

        self.meter.in_peak.max(in_peak);
        self.meter.out_peak.max(out_peak);
        self.meter.callbacks.fetch_add(1, Relaxed);
        self.meter.frames.fetch_add(frames as u64, Relaxed);
    }
}

/// Zero every output buffer, then find the first two output channels across all streams.
/// Returns them and whether the device is mono (both point at the same channel then).
#[inline(always)]
unsafe fn silenced_stereo_out(output: *mut AudioBufferList) -> Option<([Dst; 2], bool)> {
    let outs = abl_slice_mut(output);
    // Silence everything first: extra channels, and the whole output on early return.
    for b in outs.iter() {
        if !b.mData.is_null() {
            std::ptr::write_bytes(b.mData as *mut u8, 0, b.mDataByteSize as usize);
        }
    }

    let mut dst = [Dst {
        ptr: std::ptr::null_mut(),
        stride: 1,
        frames: 0,
    }; 2];
    let mut nd = 0;
    'outer: for b in outs.iter() {
        if b.mData.is_null() || b.mNumberChannels == 0 {
            continue;
        }
        let nch = b.mNumberChannels as usize;
        let frames = b.mDataByteSize as usize / 4 / nch;
        for c in 0..nch {
            dst[nd] = Dst {
                ptr: (b.mData as *mut f32).add(c),
                stride: nch,
                frames,
            };
            nd += 1;
            if nd == 2 {
                break 'outer;
            }
        }
    }
    if nd == 0 {
        return None;
    }

    let mono = nd == 1;
    if mono {
        dst[1] = dst[0];
    }
    Some((dst, mono))
}

/// The needle on its own: plays crackle and groove noise straight to the output device while
/// the tap route is idle (the tapped app is paused or closed) and "crackle without music" is on.
/// Steps aside within a few milliseconds once the main engine runs again, so the two never stack.
pub struct IdleEngine {
    params: Arc<Params>,
    meter: Arc<Meter>,
    vinyl: Vinyl,
    seen: u64,
    /// Own callbacks since the main engine last ran.
    quiet: u32,
    gain: f32,
    step: f32,
}

impl IdleEngine {
    /// Main-engine callbacks it must miss before the needle plays here instead.
    /// About a quarter of a second: also covers the main route starting up after this one.
    const QUIET_CALLBACKS: u32 = 20;

    pub fn new(params: Arc<Params>, meter: Arc<Meter>, sample_rate: f64) -> Self {
        let mut vinyl = Vinyl::new(sample_rate);
        vinyl.update(&params);
        Self {
            params,
            meter,
            vinyl,
            seen: 0,
            quiet: 0,
            gain: 0.0,
            step: (1.0 / (0.02 * sample_rate)) as f32, // 20 ms fades
        }
    }

    #[inline(always)]
    unsafe fn render(&mut self, output: *mut AudioBufferList) {
        let Some(([d0, d1], mono_out)) = silenced_stereo_out(output) else {
            return;
        };
        let frames = d0.frames.min(d1.frames);

        let main = self.meter.callbacks.load(Relaxed);
        if main != self.seen {
            self.seen = main;
            self.quiet = 0;
        } else {
            self.quiet = self.quiet.saturating_add(1);
        }
        let on = self.quiet > Self::QUIET_CALLBACKS
            && self.params.idle_surface.load(Relaxed)
            && !self.params.bypass.load(Relaxed);
        if !on && self.gain == 0.0 {
            return;
        }

        let gain = self.params.gain.get();
        self.vinyl.update(&self.params);
        self.vinyl.set_surface(1.0);
        let target = if on { 1.0 } else { 0.0 };
        let mut peak = 0f32;
        for i in 0..frames {
            self.gain = if on {
                (self.gain + self.step).min(target)
            } else {
                (self.gain - self.step).max(target)
            };
            let (l, r) = self.vinyl.tick(0.0, 0.0);
            let (l, r) = (soft_clip(l * self.gain * gain), soft_clip(r * self.gain * gain));
            peak = peak.max(l.abs()).max(r.abs());
            if mono_out {
                *d0.ptr.add(i * d0.stride) = 0.5 * (l + r);
            } else {
                *d0.ptr.add(i * d0.stride) = l;
                *d1.ptr.add(i * d1.stride) = r;
            }
        }

        self.meter.out_peak.max(peak);
        self.meter.idle_frames.fetch_add(frames as u64, Relaxed);
    }
}

/// HAL IOProc for the output device. `client` is a `*mut IdleEngine` owned by the pipeline.
pub unsafe extern "C" fn idle_io_proc(
    _dev: AudioObjectID,
    _now: *const AudioTimeStamp,
    _input: *const AudioBufferList,
    _in_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    _out_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    if !client.is_null() {
        (*(client as *mut IdleEngine)).render(output);
    }
    0
}

/// HAL IOProc. `client` is a `*mut Engine` owned by the app for the IOProc's lifetime.
pub unsafe extern "C" fn io_proc(
    _dev: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    _in_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    _out_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    if !client.is_null() {
        (*(client as *mut Engine)).render(input, output);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One 512-frame stereo block from the idle engine; returns its peak.
    fn block(e: &mut IdleEngine) -> f32 {
        let mut data = vec![0f32; 1024];
        let mut abl = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: 4096,
                mData: data.as_mut_ptr() as *mut c_void,
            }],
        };
        unsafe { e.render(&mut abl) };
        data.iter().fold(0f32, |m, x| m.max(x.abs()))
    }

    #[test]
    fn idle_needle_plays_only_while_the_main_engine_is_quiet() {
        let params = Arc::new(Params::new());
        params.set("crackle", 1.0).unwrap();
        params.set("noise", 1.0).unwrap();
        let meter = Arc::new(Meter::default());
        let mut e = IdleEngine::new(params.clone(), meter.clone(), 48_000.0);

        // Off by default.
        assert!((0..50).all(|_| block(&mut e) == 0.0));

        // On, and nothing else running: the surface plays.
        params.idle_surface.store(true, Relaxed);
        assert!((0..50).map(|_| block(&mut e)).fold(0f32, f32::max) > 0.0005);

        // Music again: the main engine ticks every block, so this one fades out and stays silent.
        for _ in 0..10 {
            meter.callbacks.fetch_add(1, Relaxed);
            block(&mut e);
        }
        assert!((0..50).all(|_| {
            meter.callbacks.fetch_add(1, Relaxed);
            block(&mut e) == 0.0
        }));
    }
}
