//! Tiny RAII layer over the CoreFoundation objects we build for the HAL.

use crate::ffi::*;
use std::os::raw::c_void;
use std::ptr;

/// Owned (+1) CoreFoundation reference, released on drop.
pub struct Cf(pub CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

pub fn string(s: &str) -> Cf {
    Cf(unsafe {
        CFStringCreateWithBytes(
            ptr::null(),
            s.as_ptr(),
            s.len() as CFIndex,
            kCFStringEncodingUTF8,
            0,
        )
    })
}

pub fn number(v: i32) -> Cf {
    Cf(unsafe {
        CFNumberCreate(
            ptr::null(),
            kCFNumberSInt32Type,
            &v as *const i32 as *const c_void,
        )
    })
}

pub fn boolean(v: bool) -> CFTypeRef {
    unsafe {
        if v {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        }
    }
}

/// Dictionary with CFType retain semantics (values may be dropped afterwards).
pub fn dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Cf {
    let keys: Vec<*const c_void> = pairs.iter().map(|p| p.0).collect();
    let vals: Vec<*const c_void> = pairs.iter().map(|p| p.1).collect();
    Cf(unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            pairs.len() as CFIndex,
            ptr::addr_of!(kCFTypeDictionaryKeyCallBacks),
            ptr::addr_of!(kCFTypeDictionaryValueCallBacks),
        )
    })
}

pub fn array(items: &[CFTypeRef]) -> Cf {
    Cf(unsafe {
        CFArrayCreate(
            ptr::null(),
            items.as_ptr(),
            items.len() as CFIndex,
            ptr::addr_of!(kCFTypeArrayCallBacks),
        )
    })
}

/// Copy a CFString (or toll-free-bridged NSString) into a Rust String.
/// Does not release `s`.
pub fn to_string(s: CFStringRef) -> String {
    if s.is_null() {
        return String::new();
    }
    unsafe {
        let len = CFStringGetLength(s);
        let cap = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
        let mut buf = vec![0u8; cap.max(1) as usize];
        if CFStringGetCString(s, buf.as_mut_ptr() as *mut _, cap, kCFStringEncodingUTF8) == 0 {
            return String::new();
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    }
}
