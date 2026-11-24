//! Core Audio HAL: devices, audio processes, process taps (macOS 14.2+),
//! private aggregate devices and the IOProc lifecycle.

use crate::cf;
use crate::ffi::*;
use std::mem::{size_of, MaybeUninit};
use std::os::raw::c_void;
use std::ptr;

pub type Result<T> = std::result::Result<T, String>;

pub fn os_err(st: OSStatus, what: &str) -> String {
    let b = (st as u32).to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        format!("{what}: OSStatus '{}' ({st})", String::from_utf8_lossy(&b))
    } else {
        format!("{what}: OSStatus {st}")
    }
}

pub fn check(st: OSStatus, what: &str) -> Result<()> {
    if st == 0 {
        Ok(())
    } else {
        Err(os_err(st, what))
    }
}

fn addr(sel: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: sel,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn get_size(obj: AudioObjectID, sel: u32, scope: u32) -> Result<u32> {
    let a = addr(sel, scope);
    let mut sz = 0u32;
    check(
        unsafe { AudioObjectGetPropertyDataSize(obj, &a, 0, ptr::null(), &mut sz) },
        "get property size",
    )?;
    Ok(sz)
}

fn get_pod<T: Copy>(obj: AudioObjectID, sel: u32, scope: u32) -> Result<T> {
    let a = addr(sel, scope);
    let mut v = MaybeUninit::<T>::zeroed();
    let mut sz = size_of::<T>() as u32;
    check(
        unsafe {
            AudioObjectGetPropertyData(
                obj,
                &a,
                0,
                ptr::null(),
                &mut sz,
                v.as_mut_ptr() as *mut c_void,
            )
        },
        "get property",
    )?;
    Ok(unsafe { v.assume_init() })
}

/// Raw property bytes in 8-byte aligned storage (for AudioBufferList etc.).
fn get_raw(obj: AudioObjectID, sel: u32, scope: u32) -> Result<Vec<u64>> {
    let sz = get_size(obj, sel, scope)?;
    let mut buf = vec![0u64; (sz as usize).div_ceil(8).max(1)];
    let a = addr(sel, scope);
    let mut io = sz;
    check(
        unsafe {
            AudioObjectGetPropertyData(
                obj,
                &a,
                0,
                ptr::null(),
                &mut io,
                buf.as_mut_ptr() as *mut c_void,
            )
        },
        "get property data",
    )?;
    Ok(buf)
}

fn get_u32_list(obj: AudioObjectID, sel: u32, scope: u32) -> Result<Vec<u32>> {
    let sz = get_size(obj, sel, scope)?;
    let mut v = vec![0u32; sz as usize / 4];
    if v.is_empty() {
        return Ok(v);
    }
    let a = addr(sel, scope);
    let mut io = sz;
    check(
        unsafe {
            AudioObjectGetPropertyData(
                obj,
                &a,
                0,
                ptr::null(),
                &mut io,
                v.as_mut_ptr() as *mut c_void,
            )
        },
        "get property list",
    )?;
    v.truncate(io as usize / 4);
    Ok(v)
}

fn get_string(obj: AudioObjectID, sel: u32, scope: u32) -> Result<String> {
    let s: CFStringRef = get_pod(obj, sel, scope)?;
    let out = cf::to_string(s);
    if !s.is_null() {
        unsafe { CFRelease(s) };
    }
    Ok(out)
}

/// Tell the HAL to dispatch notifications on its own thread: we are a CLI
/// without a CFRunLoop on the main thread.
pub fn init() {
    let a = addr(
        kAudioHardwarePropertyRunLoop,
        kAudioObjectPropertyScopeGlobal,
    );
    let null_loop: *const c_void = ptr::null();
    unsafe {
        AudioObjectSetPropertyData(
            kAudioObjectSystemObject,
            &a,
            0,
            ptr::null(),
            size_of::<*const c_void>() as u32,
            &null_loop as *const _ as *const c_void,
        );
    }
}

// ------------------------------------------------------------------ devices

#[derive(Debug, Clone)]
pub struct Device {
    pub id: AudioObjectID,
    pub name: String,
    pub uid: String,
    pub out_channels: u32,
}

pub fn stream_channels(dev: AudioObjectID, scope: u32) -> u32 {
    let Ok(raw) = get_raw(dev, kAudioDevicePropertyStreamConfiguration, scope) else {
        return 0;
    };
    unsafe {
        abl_slice(raw.as_ptr() as *const AudioBufferList)
            .iter()
            .map(|b| b.mNumberChannels)
            .sum()
    }
}

pub fn stream_count(dev: AudioObjectID, scope: u32) -> usize {
    get_size(dev, kAudioDevicePropertyStreams, scope)
        .map(|s| s as usize / 4)
        .unwrap_or(0)
}

pub fn device(id: AudioObjectID) -> Result<Device> {
    Ok(Device {
        id,
        name: get_string(
            id,
            kAudioObjectPropertyName,
            kAudioObjectPropertyScopeGlobal,
        )
        .unwrap_or_default(),
        uid: get_string(
            id,
            kAudioDevicePropertyDeviceUID,
            kAudioObjectPropertyScopeGlobal,
        )?,
        out_channels: stream_channels(id, kAudioObjectPropertyScopeOutput),
    })
}

pub fn list_devices() -> Result<Vec<Device>> {
    get_u32_list(
        kAudioObjectSystemObject,
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    )?
    .into_iter()
    .map(device)
    .collect()
}

pub fn default_output() -> Result<Device> {
    let id: AudioObjectID = get_pod(
        kAudioObjectSystemObject,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )?;
    if id == 0 {
        return Err("no default output device".into());
    }
    device(id)
}

pub fn sample_rate(dev: AudioObjectID) -> Result<f64> {
    get_pod(
        dev,
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    )
}

pub fn buffer_frames(dev: AudioObjectID) -> Result<u32> {
    get_pod(
        dev,
        kAudioDevicePropertyBufferFrameSize,
        kAudioObjectPropertyScopeGlobal,
    )
}

// ------------------------------------------------------------------ processes

#[derive(Debug, Clone)]
pub struct Process {
    pub obj: AudioObjectID,
    pub pid: i32,
    pub bundle: String,
    pub playing: bool,
}

pub fn list_processes() -> Result<Vec<Process>> {
    let g = kAudioObjectPropertyScopeGlobal;
    Ok(get_u32_list(
        kAudioObjectSystemObject,
        kAudioHardwarePropertyProcessObjectList,
        g,
    )?
    .into_iter()
    .map(|obj| Process {
        obj,
        pid: get_pod(obj, kAudioProcessPropertyPID, g).unwrap_or(-1),
        bundle: get_string(obj, kAudioProcessPropertyBundleID, g).unwrap_or_default(),
        playing: get_pod::<u32>(obj, kAudioProcessPropertyIsRunningOutput, g).unwrap_or(0) != 0,
    })
    .collect())
}

pub fn process_object_for_pid(pid: i32) -> Option<AudioObjectID> {
    let a = addr(
        kAudioHardwarePropertyTranslatePIDToProcessObject,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut out: AudioObjectID = 0;
    let mut sz = 4u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &a,
            4,
            &pid as *const i32 as *const c_void,
            &mut sz,
            &mut out as *mut _ as *mut c_void,
        )
    };
    (st == 0 && out != 0).then_some(out)
}

// ------------------------------------------------------------------ process tap

pub enum TapTarget {
    /// Everything except the listed process objects (we exclude ourselves).
    Global { exclude: Vec<AudioObjectID> },
    /// Only these process objects, mixed down to stereo.
    Processes(Vec<AudioObjectID>),
}

pub struct Tap {
    pub id: AudioObjectID,
    pub uuid: String,
    desc: Id,
}

const CATAP_MUTED_WHEN_TAPPED: isize = 2;

impl Tap {
    pub fn create(target: &TapTarget) -> Result<Tap> {
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let r = Self::create_inner(target);
            objc_autoreleasePoolPop(pool);
            r
        }
    }

    unsafe fn create_inner(target: &TapTarget) -> Result<Tap> {
        let cls = objc_getClass(c"CATapDescription".as_ptr());
        if cls.is_null() {
            return Err(
                "CATapDescription not found — process taps need macOS 14.2 or newer".into(),
            );
        }
        let (ids, init_sel) = match target {
            TapTarget::Global { exclude } => (exclude, c"initStereoGlobalTapButExcludeProcesses:"),
            TapTarget::Processes(p) => (p, c"initStereoMixdownOfProcesses:"),
        };
        let nums: Vec<cf::Cf> = ids.iter().map(|&o| cf::number(o as i32)).collect();
        let arr = cf::array(&nums.iter().map(|n| n.0).collect::<Vec<_>>());

        let desc = msg_id_id(msg_id(cls, c"alloc"), init_sel, arr.0 as Id);
        if desc.is_null() {
            return Err("CATapDescription init failed".into());
        }
        // The tapped audio no longer reaches the speakers directly; we replay it processed.
        msg_void_isize(desc, c"setMuteBehavior:", CATAP_MUTED_WHEN_TAPPED);
        msg_void_bool(desc, c"setPrivate:", YES);
        let name = cf::string("vinylizer");
        msg_void_id(desc, c"setName:", name.0 as Id);

        let uuid_obj = msg_id(desc, c"UUID");
        let uuid = cf::to_string(msg_id(uuid_obj, c"UUIDString") as CFStringRef);

        let mut id: AudioObjectID = 0;
        let st = AudioHardwareCreateProcessTap(desc, &mut id);
        if st != 0 {
            msg_void(desc, c"release");
            return Err(os_err(st, "AudioHardwareCreateProcessTap"));
        }
        Ok(Tap { id, uuid, desc })
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        unsafe {
            AudioHardwareDestroyProcessTap(self.id);
            msg_void(self.desc, c"release");
        }
    }
}

// ------------------------------------------------------------------ aggregate device

/// Private aggregate = [output device (clock master)] + [tap, drift-compensated].
/// One IOProc then sees the tapped audio as input and the speaker as output,
/// in the same clock domain: no ring buffer, no second thread, no extra copy.
pub struct Aggregate {
    pub id: AudioObjectID,
}

impl Aggregate {
    pub fn create(out_uid: &str, tap_uuid: &str) -> Result<Aggregate> {
        let k_uid = cf::string("uid");
        let v_out = cf::string(out_uid);
        let sub = cf::dict(&[(k_uid.0, v_out.0)]);
        let subs = cf::array(&[sub.0]);

        let v_tap = cf::string(tap_uuid);
        let k_drift = cf::string("drift");
        let tap = cf::dict(&[(k_uid.0, v_tap.0), (k_drift.0, cf::boolean(true))]);
        let taps = cf::array(&[tap.0]);

        let agg_uid = format!("dev.vinylizer.aggregate.{}", std::process::id());
        let (k_name, v_name) = (cf::string("name"), cf::string("Vinylizer"));
        let v_uid = cf::string(&agg_uid);
        let k_private = cf::string("private");
        let k_stacked = cf::string("stacked");
        let k_autostart = cf::string("tapautostart");
        let k_main = cf::string("master");
        let k_subs = cf::string("subdevices");
        let k_taps = cf::string("taps");

        let desc = cf::dict(&[
            (k_name.0, v_name.0),
            (k_uid.0, v_uid.0),
            (k_private.0, cf::boolean(true)),
            (k_stacked.0, cf::boolean(false)),
            (k_autostart.0, cf::boolean(true)),
            (k_main.0, v_out.0),
            (k_subs.0, subs.0),
            (k_taps.0, taps.0),
        ]);

        let mut id: AudioObjectID = 0;
        check(
            unsafe { AudioHardwareCreateAggregateDevice(desc.0, &mut id) },
            "AudioHardwareCreateAggregateDevice",
        )?;
        Ok(Aggregate { id })
    }
}

impl Drop for Aggregate {
    fn drop(&mut self) {
        unsafe {
            AudioHardwareDestroyAggregateDevice(self.id);
        }
    }
}

// ------------------------------------------------------------------ IOProc

pub struct IoProc {
    dev: AudioObjectID,
    id: AudioDeviceIOProcID,
    running: bool,
}

impl IoProc {
    pub fn new(dev: AudioObjectID, f: AudioDeviceIOProc, client: *mut c_void) -> Result<IoProc> {
        let mut id: AudioDeviceIOProcID = None;
        check(
            unsafe { AudioDeviceCreateIOProcID(dev, f, client, &mut id) },
            "AudioDeviceCreateIOProcID",
        )?;
        Ok(IoProc {
            dev,
            id,
            running: false,
        })
    }

    /// Enable only the last `enabled_tail` input streams (the tap) so a
    /// headset-style output device does not get its microphone opened
    /// (which would drop Bluetooth into low-quality HFP mode).
    pub fn restrict_input_streams(&self, total: usize, enabled_tail: usize) -> Result<()> {
        // struct { void* mIOProc; UInt32 mNumberStreams; UInt32 mStreamIsOn[n]; }
        let bytes = 8 + 4 + 4 * total;
        let mut buf = vec![0u64; bytes.div_ceil(8)];
        unsafe {
            let base = buf.as_mut_ptr() as *mut u8;
            let proc_ptr: *const c_void = self.id.map_or(ptr::null(), |f| f as *const c_void);
            ptr::write_unaligned(base as *mut *const c_void, proc_ptr);
            ptr::write_unaligned(base.add(8) as *mut u32, total as u32);
            for i in 0..total {
                let on = (i >= total - enabled_tail) as u32;
                ptr::write_unaligned(base.add(12 + 4 * i) as *mut u32, on);
            }
            let a = addr(
                kAudioDevicePropertyIOProcStreamUsage,
                kAudioObjectPropertyScopeInput,
            );
            check(
                AudioObjectSetPropertyData(
                    self.dev,
                    &a,
                    0,
                    ptr::null(),
                    bytes as u32,
                    base as *const c_void,
                ),
                "set IOProc stream usage",
            )
        }
    }

    pub fn start(&mut self) -> Result<()> {
        check(
            unsafe { AudioDeviceStart(self.dev, self.id) },
            "AudioDeviceStart",
        )?;
        self.running = true;
        Ok(())
    }

    pub fn stop(&mut self) {
        if self.running {
            unsafe { AudioDeviceStop(self.dev, self.id) };
            self.running = false;
        }
    }
}

impl Drop for IoProc {
    fn drop(&mut self) {
        self.stop();
        unsafe { AudioDeviceDestroyIOProcID(self.dev, self.id) };
    }
}
