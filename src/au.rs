#![allow(non_upper_case_globals)]
//! Audio Unit (v2) effect hosting — e.g. iZotope Vinyl.
//! The unit is rendered synchronously from the HAL IOProc thread.

use crate::cf;
use crate::ffi::*;
use crate::hal::{check, Result};
use std::mem::size_of;
use std::os::raw::c_void;
use std::ptr;

pub struct Component {
    pub name: String,
    pub desc: AudioComponentDescription,
    comp: AudioComponent,
}

fn fourcc_str(v: u32) -> String {
    String::from_utf8_lossy(&v.to_be_bytes()).into_owned()
}

impl Component {
    pub fn codes(&self) -> String {
        format!(
            "{}/{}/{}",
            fourcc_str(self.desc.componentType),
            fourcc_str(self.desc.componentSubType),
            fourcc_str(self.desc.componentManufacturer)
        )
    }
}

pub fn list_effects() -> Vec<Component> {
    let mut out = Vec::new();
    for ty in [kAudioUnitType_Effect, kAudioUnitType_MusicEffect] {
        let want = AudioComponentDescription {
            componentType: ty,
            ..Default::default()
        };
        let mut c: AudioComponent = ptr::null_mut();
        loop {
            c = unsafe { AudioComponentFindNext(c, &want) };
            if c.is_null() {
                break;
            }
            let mut name: CFStringRef = ptr::null();
            let mut desc = AudioComponentDescription::default();
            unsafe {
                AudioComponentCopyName(c, &mut name);
                AudioComponentGetDescription(c, &mut desc);
            }
            let n = cf::to_string(name);
            if !name.is_null() {
                unsafe { CFRelease(name) };
            }
            out.push(Component {
                name: n,
                desc,
                comp: c,
            });
        }
    }
    out
}

/// Input samples handed to the AU through its render callback.
struct AuInput {
    l: Box<[f32]>,
    r: Box<[f32]>,
}

#[repr(C)]
struct StereoAbl {
    n: u32,
    bufs: [AudioBuffer; 2],
}

pub struct AuPlugin {
    pub unit: AudioUnit,
    pub name: String,
    pub max_frames: usize,
    pub sample_rate: f64,
    input: Box<AuInput>,
    out_l: Box<[f32]>,
    out_r: Box<[f32]>,
    abl: StereoAbl,
    ts: AudioTimeStamp,
}

// The unit is only rendered from the IOProc thread after setup.
unsafe impl Send for AuPlugin {}

/// Handle for parameter changes from the control thread
/// (AU v2 parameter get/set is safe to call concurrently with render).
#[derive(Clone, Copy)]
pub struct AuHandle(pub AudioUnit);
unsafe impl Send for AuHandle {}
unsafe impl Sync for AuHandle {}

#[derive(Clone, Debug)]
pub struct AuParam {
    pub id: u32,
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub value: f32,
    pub default: f32,
    pub unit: u32,
    pub flags: u32,
}

impl AuParam {
    pub fn format(&self, v: f32) -> String {
        match self.unit {
            kAudioUnitParameterUnit_Decibels => format!("{v:.1} dB"),
            kAudioUnitParameterUnit_Percent => format!("{v:.0}%"),
            kAudioUnitParameterUnit_Hertz => format!("{v:.0} Hz"),
            kAudioUnitParameterUnit_Seconds => format!("{v:.2} s"),
            kAudioUnitParameterUnit_Milliseconds => format!("{v:.0} ms"),
            _ if self.max - self.min > 20.0 => format!("{v:.0}"),
            _ => format!("{v:.2}"),
        }
    }
}

unsafe extern "C" fn render_input(
    refcon: *mut c_void,
    _flags: *mut u32,
    _ts: *const AudioTimeStamp,
    _bus: u32,
    frames: u32,
    io: *mut AudioBufferList,
) -> OSStatus {
    let inp = &*(refcon as *const AuInput);
    let n = frames as usize;
    for (i, b) in abl_slice_mut(io).iter_mut().enumerate() {
        let src = if i == 0 {
            inp.l.as_ptr()
        } else {
            inp.r.as_ptr()
        };
        if b.mData.is_null() {
            b.mData = src as *mut c_void; // zero-copy: hand the AU our buffer
        } else {
            ptr::copy_nonoverlapping(src, b.mData as *mut f32, n);
        }
        b.mDataByteSize = (n * 4) as u32;
    }
    0
}

impl AuPlugin {
    pub fn open(query: &str, sample_rate: f64, max_frames: usize) -> Result<AuPlugin> {
        let q = query.to_lowercase();
        let comps = list_effects();
        let c = comps
            .iter()
            .find(|c| c.name == query)
            .or_else(|| comps.iter().find(|c| c.name.to_lowercase().contains(&q)))
            .ok_or_else(|| format!("no AU effect matching '{query}' (see --list-plugins)"))?;

        let mut unit: AudioUnit = ptr::null_mut();
        check(
            unsafe { AudioComponentInstanceNew(c.comp, &mut unit) },
            "AudioComponentInstanceNew",
        )?;

        let mut me = AuPlugin {
            unit,
            name: c.name.clone(),
            max_frames,
            sample_rate,
            input: Box::new(AuInput {
                l: vec![0.0; max_frames].into_boxed_slice(),
                r: vec![0.0; max_frames].into_boxed_slice(),
            }),
            out_l: vec![0.0; max_frames].into_boxed_slice(),
            out_r: vec![0.0; max_frames].into_boxed_slice(),
            abl: StereoAbl {
                n: 2,
                bufs: [AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: 0,
                    mData: ptr::null_mut(),
                }; 2],
            },
            ts: AudioTimeStamp {
                mFlags: kAudioTimeStampSampleTimeValid,
                ..Default::default()
            },
        };

        unsafe {
            let mf = max_frames as u32;
            check(
                AudioUnitSetProperty(
                    unit,
                    kAudioUnitProperty_MaximumFramesPerSlice,
                    kAudioUnitScope_Global,
                    0,
                    &mf as *const u32 as *const c_void,
                    4,
                ),
                "AU max frames",
            )?;
            let cb = AURenderCallbackStruct {
                inputProc: Some(render_input),
                inputProcRefCon: &*me.input as *const AuInput as *mut c_void,
            };
            check(
                AudioUnitSetProperty(
                    unit,
                    kAudioUnitProperty_SetRenderCallback,
                    kAudioUnitScope_Input,
                    0,
                    &cb as *const _ as *const c_void,
                    size_of::<AURenderCallbackStruct>() as u32,
                ),
                "AU render callback",
            )?;
        }
        me.set_formats(sample_rate)?;
        check(unsafe { AudioUnitInitialize(unit) }, "AudioUnitInitialize")?;
        me.ts.mSampleTime = 0.0;
        Ok(me)
    }

    fn set_formats(&self, sample_rate: f64) -> Result<()> {
        let fmt = AudioStreamBasicDescription {
            mSampleRate: sample_rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat
                | kAudioFormatFlagIsPacked
                | kAudioFormatFlagIsNonInterleaved,
            mBytesPerPacket: 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let fsz = size_of::<AudioStreamBasicDescription>() as u32;
        let fp = &fmt as *const _ as *const c_void;
        unsafe {
            check(
                AudioUnitSetProperty(
                    self.unit,
                    kAudioUnitProperty_StreamFormat,
                    kAudioUnitScope_Input,
                    0,
                    fp,
                    fsz,
                ),
                "AU input format",
            )?;
            check(
                AudioUnitSetProperty(
                    self.unit,
                    kAudioUnitProperty_StreamFormat,
                    kAudioUnitScope_Output,
                    0,
                    fp,
                    fsz,
                ),
                "AU output format",
            )
        }
    }

    /// Re-initialise for a new sample rate, keeping the same instance
    /// (so an open plugin window stays valid). Must not race with render.
    pub fn reconfigure(&mut self, sample_rate: f64) -> Result<()> {
        if sample_rate == self.sample_rate {
            return Ok(());
        }
        unsafe { AudioUnitUninitialize(self.unit) };
        self.set_formats(sample_rate)?;
        check(
            unsafe { AudioUnitInitialize(self.unit) },
            "AudioUnitInitialize",
        )?;
        self.sample_rate = sample_rate;
        Ok(())
    }

    pub fn handle(&self) -> AuHandle {
        AuHandle(self.unit)
    }

    /// Deinterleave `n` frames from an interleaved buffer with `ich` channels.
    ///
    /// # Safety
    /// `src` must be valid for `n * ich` reads; `n <= max_frames`.
    #[inline]
    pub unsafe fn load_input(&mut self, src: *const f32, ich: usize, n: usize) {
        let (l, r) = (self.input.l.as_mut_ptr(), self.input.r.as_mut_ptr());
        for i in 0..n {
            let s = *src.add(i * ich);
            *l.add(i) = s;
            *r.add(i) = if ich > 1 { *src.add(i * ich + 1) } else { s };
        }
    }

    /// Render `n` frames. Returns false if the AU failed (caller should pass dry).
    #[inline]
    pub fn render(&mut self, n: usize) -> bool {
        self.abl.n = 2;
        self.abl.bufs[0] = AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: (n * 4) as u32,
            mData: self.out_l.as_mut_ptr() as *mut c_void,
        };
        self.abl.bufs[1] = AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: (n * 4) as u32,
            mData: self.out_r.as_mut_ptr() as *mut c_void,
        };
        let mut flags = 0u32;
        let st = unsafe {
            AudioUnitRender(
                self.unit,
                &mut flags,
                &self.ts,
                0,
                n as u32,
                &mut self.abl as *mut StereoAbl as *mut AudioBufferList,
            )
        };
        self.ts.mSampleTime += n as f64;
        st == 0 && !self.abl.bufs[0].mData.is_null() && !self.abl.bufs[1].mData.is_null()
    }

    /// Output frame `i` of the last render (the AU may have swapped in its own buffers).
    #[inline(always)]
    pub fn out(&self, i: usize) -> (f32, f32) {
        unsafe {
            (
                *(self.abl.bufs[0].mData as *const f32).add(i),
                *(self.abl.bufs[1].mData as *const f32).add(i),
            )
        }
    }

    #[inline(always)]
    pub fn dry(&self, i: usize) -> (f32, f32) {
        (self.input.l[i], self.input.r[i])
    }
}

impl Drop for AuPlugin {
    fn drop(&mut self) {
        unsafe {
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

impl AuHandle {
    pub fn params(&self) -> Vec<AuParam> {
        let u = self.0;
        let mut sz = 0u32;
        let mut w: Boolean = 0;
        unsafe {
            if AudioUnitGetPropertyInfo(
                u,
                kAudioUnitProperty_ParameterList,
                kAudioUnitScope_Global,
                0,
                &mut sz,
                &mut w,
            ) != 0
                || sz == 0
            {
                return Vec::new();
            }
            let mut ids = vec![0u32; sz as usize / 4];
            if AudioUnitGetProperty(
                u,
                kAudioUnitProperty_ParameterList,
                kAudioUnitScope_Global,
                0,
                ids.as_mut_ptr() as *mut c_void,
                &mut sz,
            ) != 0
            {
                return Vec::new();
            }
            ids.truncate(sz as usize / 4);
            ids.into_iter()
                .filter_map(|id| {
                    let mut info: AudioUnitParameterInfo = std::mem::zeroed();
                    let mut isz = size_of::<AudioUnitParameterInfo>() as u32;
                    if AudioUnitGetProperty(
                        u,
                        kAudioUnitProperty_ParameterInfo,
                        kAudioUnitScope_Global,
                        id,
                        &mut info as *mut _ as *mut c_void,
                        &mut isz,
                    ) != 0
                    {
                        return None;
                    }
                    let name = if info.flags & kAudioUnitParameterFlag_HasCFNameString != 0
                        && !info.cfNameString.is_null()
                    {
                        let s = cf::to_string(info.cfNameString);
                        if info.flags & kAudioUnitParameterFlag_CFNameRelease != 0 {
                            CFRelease(info.cfNameString);
                        }
                        s
                    } else {
                        std::ffi::CStr::from_ptr(info.name.as_ptr())
                            .to_string_lossy()
                            .into_owned()
                    };
                    let mut value = 0f32;
                    AudioUnitGetParameter(u, id, kAudioUnitScope_Global, 0, &mut value);
                    Some(AuParam {
                        id,
                        name,
                        min: info.minValue,
                        max: info.maxValue,
                        value,
                        default: info.defaultValue,
                        unit: info.unit,
                        flags: info.flags,
                    })
                })
                .collect()
        }
    }

    pub fn set_param(&self, id: u32, value: f32) -> Result<()> {
        check(
            unsafe { AudioUnitSetParameter(self.0, id, kAudioUnitScope_Global, 0, value, 0) },
            "AudioUnitSetParameter",
        )?;
        // let the plugin's own editor (and other listeners) see the change
        let p = AudioUnitParameter {
            mAudioUnit: self.0,
            mParameterID: id,
            mScope: kAudioUnitScope_Global,
            mElement: 0,
        };
        unsafe { AUParameterListenerNotify(ptr::null_mut(), ptr::null_mut(), &p) };
        Ok(())
    }

    pub fn get_param(&self, id: u32) -> f32 {
        let mut v = 0f32;
        unsafe { AudioUnitGetParameter(self.0, id, kAudioUnitScope_Global, 0, &mut v) };
        v
    }

    /// Names for an indexed parameter's values (e.g. decades for "Year").
    pub fn value_strings(&self, id: u32) -> Vec<String> {
        unsafe {
            let mut arr: CFArrayRef = ptr::null();
            let mut sz = size_of::<CFArrayRef>() as u32;
            if AudioUnitGetProperty(
                self.0,
                kAudioUnitProperty_ParameterValueStrings,
                kAudioUnitScope_Global,
                id,
                &mut arr as *mut _ as *mut c_void,
                &mut sz,
            ) != 0
                || arr.is_null()
            {
                return Vec::new();
            }
            let v = (0..CFArrayGetCount(arr))
                .map(|i| cf::to_string(CFArrayGetValueAtIndex(arr, i)))
                .collect();
            CFRelease(arr);
            v
        }
    }

    /// Current state as an `.aupreset` (XML plist) carrying `name`.
    pub fn export_preset(&self, name: &str) -> Option<Vec<u8>> {
        unsafe {
            let mut plist: CFTypeRef = ptr::null();
            let mut sz = size_of::<CFTypeRef>() as u32;
            if AudioUnitGetProperty(
                self.0,
                kAudioUnitProperty_ClassInfo,
                kAudioUnitScope_Global,
                0,
                &mut plist as *mut _ as *mut c_void,
                &mut sz,
            ) != 0
                || plist.is_null()
            {
                return None;
            }
            let m = CFDictionaryCreateMutableCopy(ptr::null(), 0, plist);
            CFRelease(plist);
            let key = cf::string("name");
            let val = cf::string(name);
            CFDictionarySetValue(m, key.0, val.0);
            let data = CFPropertyListCreateData(
                ptr::null(),
                m,
                kCFPropertyListXMLFormat_v1_0,
                0,
                ptr::null_mut(),
            );
            CFRelease(m);
            if data.is_null() {
                return None;
            }
            let bytes =
                std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize)
                    .to_vec();
            CFRelease(data);
            Some(bytes)
        }
    }

    pub fn factory_presets(&self) -> Vec<(i32, String)> {
        let mut arr: CFArrayRef = ptr::null();
        let mut sz = size_of::<CFArrayRef>() as u32;
        let st = unsafe {
            AudioUnitGetProperty(
                self.0,
                kAudioUnitProperty_FactoryPresets,
                kAudioUnitScope_Global,
                0,
                &mut arr as *mut _ as *mut c_void,
                &mut sz,
            )
        };
        if st != 0 || arr.is_null() {
            return Vec::new();
        }
        // Ownership of this array is ambiguous across AU implementations; we deliberately don't release it.
        unsafe {
            (0..CFArrayGetCount(arr))
                .map(|i| {
                    let p = &*(CFArrayGetValueAtIndex(arr, i) as *const AUPreset);
                    (p.presetNumber, cf::to_string(p.presetName))
                })
                .collect()
        }
    }

    /// Full plugin state (what a DAW stores in a project) as a binary plist.
    pub fn save_state(&self) -> Option<Vec<u8>> {
        unsafe {
            let mut plist: CFTypeRef = ptr::null();
            let mut sz = size_of::<CFTypeRef>() as u32;
            if AudioUnitGetProperty(
                self.0,
                kAudioUnitProperty_ClassInfo,
                kAudioUnitScope_Global,
                0,
                &mut plist as *mut _ as *mut c_void,
                &mut sz,
            ) != 0
                || plist.is_null()
            {
                return None;
            }
            let data = CFPropertyListCreateData(
                ptr::null(),
                plist,
                kCFPropertyListBinaryFormat_v1_0,
                0,
                ptr::null_mut(),
            );
            CFRelease(plist);
            if data.is_null() {
                return None;
            }
            let bytes =
                std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize)
                    .to_vec();
            CFRelease(data);
            Some(bytes)
        }
    }

    pub fn load_state(&self, bytes: &[u8]) -> Result<()> {
        unsafe {
            let data = CFDataCreate(ptr::null(), bytes.as_ptr(), bytes.len() as CFIndex);
            let plist = CFPropertyListCreateWithData(
                ptr::null(),
                data,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            CFRelease(data);
            if plist.is_null() {
                return Err("corrupt plugin state".into());
            }
            let r = check(
                AudioUnitSetProperty(
                    self.0,
                    kAudioUnitProperty_ClassInfo,
                    kAudioUnitScope_Global,
                    0,
                    &plist as *const _ as *const c_void,
                    size_of::<CFTypeRef>() as u32,
                ),
                "restore plugin state",
            );
            CFRelease(plist);
            r
        }
    }

    /// The plugin's own Cocoa editor: (bundle URL, factory class name). Caller owns both CF refs.
    pub fn cocoa_ui(&self) -> Option<(CFTypeRef, CFStringRef)> {
        unsafe {
            let mut sz = 0u32;
            let mut w: Boolean = 0;
            if AudioUnitGetPropertyInfo(
                self.0,
                kAudioUnitProperty_CocoaUI,
                kAudioUnitScope_Global,
                0,
                &mut sz,
                &mut w,
            ) != 0
                || (sz as usize) < 16
            {
                return None;
            }
            let mut buf = vec![0u64; (sz as usize).div_ceil(8)];
            if AudioUnitGetProperty(
                self.0,
                kAudioUnitProperty_CocoaUI,
                kAudioUnitScope_Global,
                0,
                buf.as_mut_ptr() as *mut c_void,
                &mut sz,
            ) != 0
            {
                return None;
            }
            let url = buf[0] as usize as CFTypeRef;
            let cls = buf[1] as usize as CFStringRef;
            (!url.is_null() && !cls.is_null()).then_some((url, cls))
        }
    }

    pub fn set_preset(&self, number: i32) -> Result<()> {
        let name = self
            .factory_presets()
            .into_iter()
            .find(|p| p.0 == number)
            .map(|p| p.1)
            .ok_or_else(|| format!("no factory preset #{number}"))?;
        let cfname = cf::string(&name);
        let p = AUPreset {
            presetNumber: number,
            presetName: cfname.0,
        };
        check(
            unsafe {
                AudioUnitSetProperty(
                    self.0,
                    kAudioUnitProperty_PresentPreset,
                    kAudioUnitScope_Global,
                    0,
                    &p as *const _ as *const c_void,
                    size_of::<AUPreset>() as u32,
                )
            },
            "set AU preset",
        )
    }
}
