//! Hand-written, minimal bindings to CoreFoundation, CoreAudio (HAL),
//! AudioToolbox (AU hosting) and the Objective-C runtime.
//! Only what vinylizer needs; layouts mirror the macOS 14.2+ SDK headers.
#![allow(
    non_upper_case_globals,
    non_snake_case,
    non_camel_case_types,
    dead_code
)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};

pub type OSStatus = i32;
pub type AudioObjectID = u32;
pub type Boolean = u8;
pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFIndex = isize;
pub type AudioComponent = *mut c_void;
pub type AudioUnit = *mut c_void;

#[repr(C)]
pub struct Opaque {
    _p: [u8; 0],
}

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

// ---------------------------------------------------------------- CoreFoundation
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
pub const kCFNumberSInt32Type: CFIndex = 3;
pub const kCFPropertyListBinaryFormat_v1_0: CFIndex = 200;
pub const kCFPropertyListXMLFormat_v1_0: CFIndex = 100;

#[cfg_attr(target_os = "macos", link(name = "CoreFoundation", kind = "framework"))]
extern "C" {
    pub static kCFBooleanTrue: CFTypeRef;
    pub static kCFBooleanFalse: CFTypeRef;
    pub static kCFTypeDictionaryKeyCallBacks: Opaque;
    pub static kCFTypeDictionaryValueCallBacks: Opaque;
    pub static kCFTypeArrayCallBacks: Opaque;

    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        n: CFIndex,
        enc: u32,
        external: Boolean,
    ) -> CFStringRef;
    pub fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    pub fn CFStringGetMaximumSizeForEncoding(len: CFIndex, enc: u32) -> CFIndex;
    pub fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, enc: u32)
        -> Boolean;
    pub fn CFNumberCreate(alloc: CFAllocatorRef, ty: CFIndex, value: *const c_void) -> CFTypeRef;
    pub fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        n: CFIndex,
        kcb: *const Opaque,
        vcb: *const Opaque,
    ) -> CFDictionaryRef;
    pub fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const *const c_void,
        n: CFIndex,
        cb: *const Opaque,
    ) -> CFArrayRef;
    pub fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
    pub fn CFDictionaryCreateMutableCopy(
        alloc: CFAllocatorRef,
        capacity: CFIndex,
        dict: CFTypeRef,
    ) -> CFTypeRef;
    pub fn CFDictionarySetValue(dict: CFTypeRef, key: *const c_void, value: *const c_void);
    pub fn CFDataCreate(alloc: CFAllocatorRef, bytes: *const u8, len: CFIndex) -> CFTypeRef;
    pub fn CFDataGetLength(d: CFTypeRef) -> CFIndex;
    pub fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
    pub fn CFPropertyListCreateData(
        alloc: CFAllocatorRef,
        plist: CFTypeRef,
        format: CFIndex,
        options: usize,
        error: *mut CFTypeRef,
    ) -> CFTypeRef;
    pub fn CFPropertyListCreateWithData(
        alloc: CFAllocatorRef,
        data: CFTypeRef,
        options: usize,
        format: *mut CFIndex,
        error: *mut CFTypeRef,
    ) -> CFTypeRef;
    pub fn CFArrayGetValueAtIndex(a: CFArrayRef, i: CFIndex) -> *const c_void;
}

// ---------------------------------------------------------------- CoreAudio types
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: u32,
    pub mScope: u32,
    pub mElement: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

/// Variable-length in C (`mBuffers[mNumberBuffers]`); access via `abl_slice`.
#[repr(C)]
pub struct AudioBufferList {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SMPTETime {
    pub mSubframes: i16,
    pub mSubframeDivisor: i16,
    pub mCounter: u32,
    pub mType: u32,
    pub mFlags: u32,
    pub mHours: i16,
    pub mMinutes: i16,
    pub mSeconds: i16,
    pub mFrames: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct AudioTimeStamp {
    pub mSampleTime: f64,
    pub mHostTime: u64,
    pub mRateScalar: f64,
    pub mWordClockTime: u64,
    pub mSMPTETime: SMPTETime,
    pub mFlags: u32,
    pub mReserved: u32,
}
pub const kAudioTimeStampSampleTimeValid: u32 = 1;

pub type AudioDeviceIOProc = unsafe extern "C" fn(
    AudioObjectID,
    *const AudioTimeStamp,
    *const AudioBufferList,
    *const AudioTimeStamp,
    *mut AudioBufferList,
    *const AudioTimeStamp,
    *mut c_void,
) -> OSStatus;
pub type AudioDeviceIOProcID = Option<AudioDeviceIOProc>;

// ---------------------------------------------------------------- CoreAudio constants
pub const kAudioObjectSystemObject: AudioObjectID = 1;
pub const kAudioObjectPropertyElementMain: u32 = 0;
pub const kAudioObjectPropertyScopeGlobal: u32 = fourcc(b"glob");
pub const kAudioObjectPropertyScopeInput: u32 = fourcc(b"inpt");
pub const kAudioObjectPropertyScopeOutput: u32 = fourcc(b"outp");
pub const kAudioObjectPropertyName: u32 = fourcc(b"lnam");

pub const kAudioHardwarePropertyDevices: u32 = fourcc(b"dev#");
pub const kAudioHardwarePropertyDefaultOutputDevice: u32 = fourcc(b"dOut");
pub const kAudioHardwarePropertyRunLoop: u32 = fourcc(b"rnlp");
pub const kAudioHardwarePropertyProcessObjectList: u32 = fourcc(b"prs#");
pub const kAudioHardwarePropertyTranslatePIDToProcessObject: u32 = fourcc(b"id2p");

pub const kAudioProcessPropertyPID: u32 = fourcc(b"ppid");
pub const kAudioProcessPropertyBundleID: u32 = fourcc(b"pbid");
pub const kAudioProcessPropertyIsRunningOutput: u32 = fourcc(b"piro");

pub const kAudioDevicePropertyDeviceUID: u32 = fourcc(b"uid ");
pub const kAudioDevicePropertyStreams: u32 = fourcc(b"stm#");
pub const kAudioDevicePropertyStreamConfiguration: u32 = fourcc(b"slay");
pub const kAudioDevicePropertyNominalSampleRate: u32 = fourcc(b"nsrt");
pub const kAudioDevicePropertyBufferFrameSize: u32 = fourcc(b"fsiz");
pub const kAudioDevicePropertyIOProcStreamUsage: u32 = fourcc(b"suse");
pub const kAudioTapPropertyFormat: u32 = fourcc(b"tfmt");

pub const kAudioFormatLinearPCM: u32 = fourcc(b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub const kAudioFormatFlagIsPacked: u32 = 1 << 3;
pub const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;

#[cfg_attr(target_os = "macos", link(name = "CoreAudio", kind = "framework"))]
extern "C" {
    pub fn AudioObjectGetPropertyDataSize(
        id: AudioObjectID,
        addr: *const AudioObjectPropertyAddress,
        qual_size: u32,
        qual: *const c_void,
        out_size: *mut u32,
    ) -> OSStatus;
    pub fn AudioObjectGetPropertyData(
        id: AudioObjectID,
        addr: *const AudioObjectPropertyAddress,
        qual_size: u32,
        qual: *const c_void,
        io_size: *mut u32,
        out: *mut c_void,
    ) -> OSStatus;
    pub fn AudioObjectSetPropertyData(
        id: AudioObjectID,
        addr: *const AudioObjectPropertyAddress,
        qual_size: u32,
        qual: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> OSStatus;
    pub fn AudioHardwareCreateAggregateDevice(
        desc: CFDictionaryRef,
        out: *mut AudioObjectID,
    ) -> OSStatus;
    pub fn AudioHardwareDestroyAggregateDevice(id: AudioObjectID) -> OSStatus;
    /// `desc` is a `CATapDescription *`.
    pub fn AudioHardwareCreateProcessTap(desc: *mut c_void, out: *mut AudioObjectID) -> OSStatus;
    pub fn AudioHardwareDestroyProcessTap(id: AudioObjectID) -> OSStatus;
    pub fn AudioDeviceCreateIOProcID(
        dev: AudioObjectID,
        proc_: AudioDeviceIOProc,
        client: *mut c_void,
        out: *mut AudioDeviceIOProcID,
    ) -> OSStatus;
    pub fn AudioDeviceDestroyIOProcID(dev: AudioObjectID, id: AudioDeviceIOProcID) -> OSStatus;
    pub fn AudioDeviceStart(dev: AudioObjectID, id: AudioDeviceIOProcID) -> OSStatus;
    pub fn AudioDeviceStop(dev: AudioObjectID, id: AudioDeviceIOProcID) -> OSStatus;
}

// ---------------------------------------------------------------- AudioToolbox (AU v2 hosting)
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct AudioComponentDescription {
    pub componentType: u32,
    pub componentSubType: u32,
    pub componentManufacturer: u32,
    pub componentFlags: u32,
    pub componentFlagsMask: u32,
}

pub type AURenderCallback = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *const AudioTimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> OSStatus;

#[repr(C)]
pub struct AURenderCallbackStruct {
    pub inputProc: Option<AURenderCallback>,
    pub inputProcRefCon: *mut c_void,
}

#[repr(C)]
pub struct AudioUnitParameterInfo {
    pub name: [c_char; 52],
    pub unitName: CFStringRef,
    pub clumpID: u32,
    pub cfNameString: CFStringRef,
    pub unit: u32,
    pub minValue: f32,
    pub maxValue: f32,
    pub defaultValue: f32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AUPreset {
    pub presetNumber: i32,
    pub presetName: CFStringRef,
}

pub const kAudioUnitType_Effect: u32 = fourcc(b"aufx");
pub const kAudioUnitType_MusicEffect: u32 = fourcc(b"aumf");
pub const kAudioUnitScope_Global: u32 = 0;
pub const kAudioUnitScope_Input: u32 = 1;
pub const kAudioUnitScope_Output: u32 = 2;
pub const kAudioUnitProperty_ClassInfo: u32 = 0;
pub const kAudioUnitProperty_ParameterList: u32 = 3;
pub const kAudioUnitProperty_CocoaUI: u32 = 31;
pub const kAudioUnitProperty_ParameterValueStrings: u32 = 16;
pub const kAudioUnitParameterFlag_IsReadable: u32 = 1 << 30;
pub const kAudioUnitParameterFlag_IsWritable: u32 = 1 << 31;
pub const kAudioUnitParameterUnit_Indexed: u32 = 1;
pub const kAudioUnitParameterUnit_Boolean: u32 = 2;
pub const kAudioUnitParameterUnit_Percent: u32 = 3;
pub const kAudioUnitParameterUnit_Seconds: u32 = 4;
pub const kAudioUnitParameterUnit_Hertz: u32 = 8;
pub const kAudioUnitParameterUnit_Decibels: u32 = 13;
pub const kAudioUnitParameterUnit_Milliseconds: u32 = 24;

#[repr(C)]
pub struct AudioUnitParameter {
    pub mAudioUnit: AudioUnit,
    pub mParameterID: u32,
    pub mScope: u32,
    pub mElement: u32,
}
pub const kAudioUnitProperty_ParameterInfo: u32 = 4;
pub const kAudioUnitProperty_StreamFormat: u32 = 8;
pub const kAudioUnitProperty_MaximumFramesPerSlice: u32 = 14;
pub const kAudioUnitProperty_SetRenderCallback: u32 = 23;
pub const kAudioUnitProperty_FactoryPresets: u32 = 24;
pub const kAudioUnitProperty_PresentPreset: u32 = 36;
pub const kAudioUnitParameterFlag_CFNameRelease: u32 = 1 << 4;
pub const kAudioUnitParameterFlag_HasCFNameString: u32 = 1 << 27;

#[cfg_attr(target_os = "macos", link(name = "AudioToolbox", kind = "framework"))]
extern "C" {
    pub fn AudioComponentFindNext(
        prev: AudioComponent,
        desc: *const AudioComponentDescription,
    ) -> AudioComponent;
    pub fn AudioComponentCopyName(c: AudioComponent, out: *mut CFStringRef) -> OSStatus;
    pub fn AudioComponentGetDescription(
        c: AudioComponent,
        out: *mut AudioComponentDescription,
    ) -> OSStatus;
    pub fn AudioComponentInstanceNew(c: AudioComponent, out: *mut AudioUnit) -> OSStatus;
    pub fn AudioComponentInstanceDispose(u: AudioUnit) -> OSStatus;
    pub fn AudioUnitInitialize(u: AudioUnit) -> OSStatus;
    pub fn AudioUnitUninitialize(u: AudioUnit) -> OSStatus;
    pub fn AudioUnitSetProperty(
        u: AudioUnit,
        id: u32,
        scope: u32,
        elem: u32,
        data: *const c_void,
        size: u32,
    ) -> OSStatus;
    pub fn AudioUnitGetProperty(
        u: AudioUnit,
        id: u32,
        scope: u32,
        elem: u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> OSStatus;
    pub fn AudioUnitGetPropertyInfo(
        u: AudioUnit,
        id: u32,
        scope: u32,
        elem: u32,
        size: *mut u32,
        writable: *mut Boolean,
    ) -> OSStatus;
    pub fn AudioUnitRender(
        u: AudioUnit,
        flags: *mut u32,
        ts: *const AudioTimeStamp,
        bus: u32,
        frames: u32,
        io: *mut AudioBufferList,
    ) -> OSStatus;
    pub fn AudioUnitGetParameter(
        u: AudioUnit,
        id: u32,
        scope: u32,
        elem: u32,
        out: *mut f32,
    ) -> OSStatus;
    pub fn AudioUnitSetParameter(
        u: AudioUnit,
        id: u32,
        scope: u32,
        elem: u32,
        value: f32,
        offset: u32,
    ) -> OSStatus;
    pub fn AUParameterListenerNotify(
        listener: *mut c_void,
        object: *mut c_void,
        param: *const AudioUnitParameter,
    ) -> OSStatus;
}

// Foundation must be loaded for NSUUID/NSString used via CATapDescription;
// AppKit for the menu bar UI; CoreAudioKit for AUGenericView (plugin UI fallback).
#[cfg_attr(target_os = "macos", link(name = "Foundation", kind = "framework"))]
extern "C" {}
#[cfg_attr(target_os = "macos", link(name = "AppKit", kind = "framework"))]
extern "C" {}
#[cfg_attr(target_os = "macos", link(name = "CoreAudioKit", kind = "framework"))]
extern "C" {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NSPoint {
    pub x: f64,
    pub y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NSSize {
    pub width: f64,
    pub height: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NSRect {
    pub origin: NSPoint,
    pub size: NSSize,
}
impl NSRect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self {
            origin: NSPoint { x, y },
            size: NSSize {
                width: w,
                height: h,
            },
        }
    }
}

// ---------------------------------------------------------------- Objective-C runtime
pub type Id = *mut c_void;
pub type Sel = *mut c_void;
#[cfg(target_arch = "aarch64")]
pub type ObjcBool = bool;
#[cfg(not(target_arch = "aarch64"))]
pub type ObjcBool = i8;
#[cfg(target_arch = "aarch64")]
pub const YES: ObjcBool = true;
#[cfg(not(target_arch = "aarch64"))]
pub const YES: ObjcBool = 1;

#[cfg_attr(target_os = "macos", link(name = "objc"))]
extern "C" {
    pub fn objc_getClass(name: *const c_char) -> Id;
    pub fn sel_registerName(name: *const c_char) -> Sel;
    pub fn objc_msgSend();
    pub fn objc_autoreleasePoolPush() -> *mut c_void;
    pub fn objc_autoreleasePoolPop(pool: *mut c_void);
    pub fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra: usize) -> Id;
    pub fn objc_registerClassPair(cls: Id);
    pub fn class_addMethod(cls: Id, sel: Sel, imp: *const c_void, types: *const c_char) -> u8;
    pub fn objc_getProtocol(name: *const c_char) -> Id;
    pub fn class_addProtocol(cls: Id, proto: Id) -> u8;
}

/// Typed objc_msgSend: `send!(obj, "setTag:", 3isize => isize; -> ())`.
#[macro_export]
macro_rules! send {
    ($obj:expr, $sel:literal $(, $a:expr => $t:ty)* ; -> $r:ty) => {{
        #[allow(clippy::missing_transmute_annotations)]
        let f: unsafe extern "C" fn($crate::ffi::Id, $crate::ffi::Sel $(, $t)*) -> $r =
            std::mem::transmute($crate::ffi::objc_msgSend as unsafe extern "C" fn());
        f($obj, $crate::ffi::sel_registerName(concat!($sel, "\0").as_ptr() as *const std::os::raw::c_char) $(, $a)*)
    }};
}

/// Objective-C class by name.
pub unsafe fn class(name: &CStr) -> Id {
    objc_getClass(name.as_ptr())
}

/// objc_msgSend must be called through a correctly-typed function pointer
/// (mandatory on arm64, where it is not variadic).
macro_rules! msg_fn {
    ($name:ident, ($($arg:ident : $ty:ty),*) -> $ret:ty) => {
        pub unsafe fn $name(obj: Id, sel_name: &CStr $(, $arg: $ty)*) -> $ret {
            let f: unsafe extern "C" fn(Id, Sel $(, $ty)*) -> $ret =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(obj, sel_registerName(sel_name.as_ptr()) $(, $arg)*)
        }
    };
}
msg_fn!(msg_id, () -> Id);
msg_fn!(msg_id_id, (a: Id) -> Id);
msg_fn!(msg_void, () -> ());
msg_fn!(msg_void_id, (a: Id) -> ());
msg_fn!(msg_void_isize, (a: isize) -> ());
msg_fn!(msg_void_bool, (a: ObjcBool) -> ());

// ---------------------------------------------------------------- helpers
/// View an AudioBufferList as a slice of AudioBuffers.
///
/// # Safety
/// `abl` must be null or point to a valid AudioBufferList.
#[inline(always)]
pub unsafe fn abl_slice<'a>(abl: *const AudioBufferList) -> &'a [AudioBuffer] {
    if abl.is_null() {
        return &[];
    }
    std::slice::from_raw_parts(
        std::ptr::addr_of!((*abl).mBuffers) as *const AudioBuffer,
        (*abl).mNumberBuffers as usize,
    )
}

/// # Safety
/// `abl` must be null or point to a valid, writable AudioBufferList.
#[inline(always)]
pub unsafe fn abl_slice_mut<'a>(abl: *mut AudioBufferList) -> &'a mut [AudioBuffer] {
    if abl.is_null() {
        return &mut [];
    }
    std::slice::from_raw_parts_mut(
        std::ptr::addr_of_mut!((*abl).mBuffers) as *mut AudioBuffer,
        (*abl).mNumberBuffers as usize,
    )
}

#[cfg(test)]
mod layout {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn struct_layouts_match_sdk() {
        assert_eq!(size_of::<AudioStreamBasicDescription>(), 40);
        assert_eq!(size_of::<AudioBuffer>(), 16);
        assert_eq!(offset_of!(AudioBufferList, mBuffers), 8);
        assert_eq!(size_of::<SMPTETime>(), 24);
        assert_eq!(size_of::<AudioTimeStamp>(), 64);
        assert_eq!(size_of::<AudioComponentDescription>(), 20);
        assert_eq!(offset_of!(AudioUnitParameterInfo, unitName), 56);
        assert_eq!(offset_of!(AudioUnitParameterInfo, cfNameString), 72);
        assert_eq!(offset_of!(AudioUnitParameterInfo, flags), 96);
        assert_eq!(size_of::<AudioUnitParameterInfo>(), 104);
        assert_eq!(size_of::<AUPreset>(), 16);
        assert_eq!(fourcc(b"glob"), 0x676c_6f62);
    }
}
