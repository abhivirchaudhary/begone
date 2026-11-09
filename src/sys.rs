//! The few things that need the C library: free disk space and the local time.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// `(total, free)` bytes of the volume holding `path`.
// The block-size fields are different integer types on different platforms.
#[allow(clippy::useless_conversion)]
pub fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` only writes into the zeroed struct we hand it.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let frsize = u64::from(s.f_frsize);
    Some((u64::from(s.f_blocks) * frsize, u64::from(s.f_bavail) * frsize))
}

/// What Finder calls available on the volume holding `path`: the free space plus the purgeable space (local snapshots, caches
/// macOS clears itself when it needs room). `None` where the system does not say.
pub fn available_with_purgeable(path: &Path) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::c_void;
        type CFIndex = isize;
        type CFRef = *const c_void;
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFRef;
            fn CFURLCreateFromFileSystemRepresentation(allocator: CFRef, buffer: *const u8, length: CFIndex, is_directory: u8) -> CFRef;
            fn CFURLCopyResourcePropertyForKey(url: CFRef, key: CFRef, value: *mut CFRef, error: *mut CFRef) -> u8;
            fn CFNumberGetValue(number: CFRef, kind: CFIndex, value: *mut c_void) -> u8;
            fn CFRelease(thing: CFRef);
        }
        const SINT64: CFIndex = 4;
        let bytes = path.as_os_str().as_bytes();
        // SAFETY: the URL is made from a buffer that outlives the call and released after it; the property comes back retained
        // and is released once read; `CFNumberGetValue` writes one 64-bit integer into the integer handed to it.
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as CFIndex, 1);
            if url.is_null() {
                return None;
            }
            let mut value: CFRef = std::ptr::null();
            let ok =
                CFURLCopyResourcePropertyForKey(url, kCFURLVolumeAvailableCapacityForImportantUsageKey, &mut value, std::ptr::null_mut());
            CFRelease(url);
            if ok == 0 || value.is_null() {
                return None;
            }
            let mut number: i64 = 0;
            let read = CFNumberGetValue(value, SINT64, std::ptr::addr_of_mut!(number).cast());
            CFRelease(value);
            (read != 0 && number >= 0).then_some(number as u64)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        None
    }
}

/// `Sat 7 Nov 2026 14:32` in the local time zone.
pub fn local_stamp(epoch_secs: i64) -> String {
    // SAFETY: `localtime_r` and `strftime` write only into buffers we own and size.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = epoch_secs as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = [0u8; 64];
        let fmt = c"%a %e %b %Y %H:%M";
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Show a path in Finder, selected. Read-only: it only asks Finder to look. Never fails loudly, since a
/// missing Finder or a path that has gone is not worth interrupting anyone for.
pub fn reveal(path: &Path) {
    // An absolute path never starts with a dash, so it cannot be mistaken for an option.
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(&abs)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(not(target_os = "macos"))]
    let _ = abs;
}

pub fn now_epoch() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_finder_counts_as_available_is_at_least_what_is_free_on_the_volume() {
        let (total, free) = disk_usage(Path::new("/")).expect("the root volume can be read");
        let available = available_with_purgeable(Path::new("/"));
        if cfg!(target_os = "macos") {
            let available = available.expect("macOS says how much is available");
            assert!(available <= total, "{available} of {total}");
            // The free space can change between the two readings, so allow for a little.
            assert!(available + 2_000_000_000 >= free, "{available} available, {free} free");
        } else {
            assert_eq!(available, None);
        }
        assert_eq!(available_with_purgeable(Path::new("/nonexistent/folder")).filter(|_| !cfg!(target_os = "macos")), None);
        assert!(available_with_purgeable(Path::new("")).is_none() || cfg!(target_os = "macos"));
    }

    #[test]
    fn the_temp_volume_reports_sane_numbers() {
        let (total, free) = disk_usage(&std::env::temp_dir()).expect("statvfs works");
        assert!(total > 0 && free <= total);
    }

    #[test]
    fn a_stamp_reads_like_a_date_with_single_spaces() {
        let s = local_stamp(1_700_000_000);
        assert!(s.contains("2023") && !s.contains("  "), "{s}");
    }
}
