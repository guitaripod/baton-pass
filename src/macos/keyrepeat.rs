//! The key repeat delay macOS applies to every keyboard, the virtual one included: how long a
//! key may stay down before it starts repeating.

use std::ffi::{CString, c_char, c_void};
use std::time::Duration;

type CFTypeRef = *const c_void;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: CFTypeRef, allocator: CFTypeRef, options: u32) -> CFTypeRef;
    fn IOObjectRelease(object: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: CFTypeRef, text: *const c_char, encoding: u32) -> CFTypeRef;
    fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFNumberGetValue(number: CFTypeRef, kind: isize, value: *mut c_void) -> bool;
    fn CFNumberGetTypeID() -> usize;
    fn CFGetTypeID(value: CFTypeRef) -> usize;
    fn CFRelease(value: CFTypeRef);
}

const UTF8: u32 = 0x0800_0100;
const SINT64: isize = 4;

/// The initial key repeat delay from IOHIDSystem's HIDParameters, which the login session keeps
/// in step with the Keyboard settings (InitialKeyRepeat 10 is 166 ms there).
pub fn initial_delay() -> Option<Duration> {
    let name = CString::new("IOHIDSystem").ok()?;
    unsafe {
        let service = IOServiceGetMatchingService(0, IOServiceMatching(name.as_ptr()));
        if service == 0 {
            return None;
        }
        let parameters_key = cf_string("HIDParameters")?;
        let parameters = IORegistryEntryCreateCFProperty(service, parameters_key, std::ptr::null(), 0);
        CFRelease(parameters_key);
        IOObjectRelease(service);
        if parameters.is_null() {
            return None;
        }
        let nanoseconds = read_i64(parameters, "HIDInitialKeyRepeat");
        CFRelease(parameters);
        nanoseconds.filter(|value| *value > 0).map(|value| Duration::from_nanos(value as u64))
    }
}

/// Reads a number out of a CoreFoundation dictionary, checking both types on the way.
unsafe fn read_i64(dictionary: CFTypeRef, key: &str) -> Option<i64> {
    unsafe {
        if CFGetTypeID(dictionary) != CFDictionaryGetTypeID() {
            return None;
        }
        let key = cf_string(key)?;
        let number = CFDictionaryGetValue(dictionary, key);
        CFRelease(key);
        if number.is_null() || CFGetTypeID(number) != CFNumberGetTypeID() {
            return None;
        }
        let mut value: i64 = 0;
        CFNumberGetValue(number, SINT64, (&mut value as *mut i64).cast()).then_some(value)
    }
}

/// An owned CFString; the caller releases it.
fn cf_string(text: &str) -> Option<CFTypeRef> {
    let text = CString::new(text).ok()?;
    let string = unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) };
    (!string.is_null()).then_some(string)
}
