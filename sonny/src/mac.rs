//! The Mac's own frameworks, called in the process where Sonny would
//! otherwise start a program for each: the appearance, read through
//! CoreFoundation as `defaults` reads it, and a sound played through
//! AudioToolbox as `afplay` plays it. Every window asks for the appearance
//! every few seconds, and a program started that often is noise to the
//! security software that watches each one start.
use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

type CFTypeRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFPreferencesAnyApplication: CFTypeRef;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        external: u8,
    ) -> CFTypeRef;
    fn CFPreferencesAppSynchronize(app: CFTypeRef) -> u8;
    fn CFPreferencesCopyAppValue(key: CFTypeRef, app: CFTypeRef) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetCString(string: CFTypeRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFURLCreateFromFileSystemRepresentation(
        alloc: CFTypeRef,
        path: *const u8,
        len: isize,
        directory: u8,
    ) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioServicesCreateSystemSoundID(url: CFTypeRef, id: *mut u32) -> i32;
    fn AudioServicesPlaySystemSound(id: u32);
}

const UTF8: u32 = 0x0800_0100;

/// Whether the Mac is in Dark Mode: the global `AppleInterfaceStyle` says
/// `Dark`, read fresh each time.
pub fn dark() -> bool {
    let name = b"AppleInterfaceStyle";
    // SAFETY: each CoreFoundation object made or copied here is checked
    // for null before use and released once; the buffer outlives the call
    // that fills it, and its size is passed with it.
    unsafe {
        let key = CFStringCreateWithBytes(
            std::ptr::null(),
            name.as_ptr(),
            name.len() as isize,
            UTF8,
            0,
        );
        if key.is_null() {
            return false;
        }
        CFPreferencesAppSynchronize(kCFPreferencesAnyApplication);
        let value = CFPreferencesCopyAppValue(key, kCFPreferencesAnyApplication);
        CFRelease(key);
        if value.is_null() {
            return false;
        }
        let mut buf = [0 as c_char; 16];
        let dark = CFGetTypeID(value) == CFStringGetTypeID()
            && CFStringGetCString(value, buf.as_mut_ptr(), buf.len() as isize, UTF8) != 0
            && CStr::from_ptr(buf.as_ptr()).to_bytes() == b"Dark";
        CFRelease(value);
        dark
    }
}

/// Each sound file made a system sound once, kept for the next time.
static SOUNDS: Mutex<Vec<(PathBuf, u32)>> = Mutex::new(Vec::new());

/// Play the sound file at `path` and return at once; false when the Mac
/// cannot make it a sound.
pub fn play(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut sounds = SOUNDS.lock().unwrap_or_else(|e| e.into_inner());
    let id = match sounds.iter().find(|(known, _)| known == path) {
        Some((_, id)) => *id,
        None => {
            let bytes = path.as_os_str().as_bytes();
            let mut id = 0;
            // SAFETY: the URL is checked for null, released once, and the
            // path's bytes outlive the call that reads them.
            let made = unsafe {
                let url = CFURLCreateFromFileSystemRepresentation(
                    std::ptr::null(),
                    bytes.as_ptr(),
                    bytes.len() as isize,
                    0,
                );
                if url.is_null() {
                    return false;
                }
                let status = AudioServicesCreateSystemSoundID(url, &mut id);
                CFRelease(url);
                status == 0
            };
            if !made {
                return false;
            }
            sounds.push((path.to_owned(), id));
            id
        }
    };
    // SAFETY: `id` is a system sound AudioServicesCreateSystemSoundID made
    // and nothing disposed of.
    unsafe { AudioServicesPlaySystemSound(id) };
    true
}
