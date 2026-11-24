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
use vinylizer::dsp::Vinyl;
use vinylizer::params::{AtomicF32, Params};

#[derive(Default)]
pub struct Meter {
    pub in_peak: AtomicF32,
    pub out_peak: AtomicF32,
    pub callbacks: AtomicU64,
    pub frames: AtomicU64,
}

pub struct Engine {
    params: Arc<Params>,
    meter: Arc<Meter>,
    vinyl: Vinyl,
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
            au,
            chain_builtin,
        }
    }

    #[inline(always)]
    unsafe fn render(&mut self, input: *const AudioBufferList, output: *mut AudioBufferList) {
        let outs = abl_slice_mut(output);
        // Silence everything first: extra channels, and the whole output on early return.
        for b in outs.iter() {
            if !b.mData.is_null() {
                std::ptr::write_bytes(b.mData as *mut u8, 0, b.mDataByteSize as usize);
            }
        }

        // First two output channels across all output streams.
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
            return;
        }
        let mono_out = nd == 1;
        if mono_out {
            dst[1] = dst[0];
        }

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
        self.vinyl.update(&self.params);

        let (d0, d1) = (dst[0], dst[1]);
        let mut in_peak = 0f32;
        let mut out_peak = 0f32;
        let mut write = |i: usize, l: f32, r: f32| {
            let (l, r) = (l * gain, r * gain);
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
                    if self.chain_builtin {
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
