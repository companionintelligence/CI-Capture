//! Bundle id → human display name, so a fact reads "Used Google Chrome for
//! 12 min" rather than "Used com.google.Chrome for 12 min". CI-Server
//! search-indexes `title`, and nobody searches their memory for
//! `com.hammerandchisel.discord`.
//!
//! On macOS the name comes from Launch Services (`NSWorkspace
//! URLForApplicationWithBundleIdentifier:` → `NSFileManager
//! displayNameAtPath:`), which is what Finder shows and is localized the
//! way the user sees it. Every lookup is cached for the life of the
//! process — including misses, because an uninstalled app's bundle id
//! recurs on every row of a backfill and each miss is a Launch Services
//! round trip. Off macOS, and for anything Launch Services does not know,
//! the fallback is the id's last dotted segment (`Chrome`, `discord`),
//! which is at least readable.
//!
//! Callers must pass **real bundle ids only**. The `/notification/usage`
//! pseudo-bundles — `_WEB_CENTER_:<host>`, `_SYSTEM_CENTER_:<daemon>`, and
//! any other `_`-prefixed value — are handled in `facts` before this is
//! reached; sending a host name through a Launch Services query would be
//! pointless and would put a web domain in the cache keys.

/// The display name for `bundle_id`, falling back to its last dotted
/// segment (`com.example.NotInstalled` → `NotInstalled`) when the platform
/// cannot resolve it.
pub fn app_name(bundle_id: &str) -> String {
    imp::display_name(bundle_id).unwrap_or_else(|| last_segment(bundle_id))
}

/// `com.google.Chrome` → `Chrome`; an id without a dot, or one that ends in
/// a dot, is returned whole rather than as an empty string.
fn last_segment(bundle_id: &str) -> String {
    bundle_id
        .rsplit('.')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(bundle_id)
        .to_owned()
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashMap;
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::sync::{Mutex, OnceLock};

    use objc::runtime::{Class, Object};
    use objc::{msg_send, sel, sel_impl};

    /// Process-wide cache, misses included. A `Mutex` rather than a lock-free
    /// map because contention is nil: the lane maps rows from one task
    /// (`drain_once`, which also runs `digest_once`) on a Tokio runtime
    /// worker thread, and the Settings process never calls this at all. The
    /// guard is held across the Launch Services call — at most one round
    /// trip per distinct bundle id per process, never across an `.await`.
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();

    pub(super) fn display_name(bundle_id: &str) -> Option<String> {
        let cache = CACHE.get_or_init(Mutex::default);
        // A poisoned lock only means another thread panicked mid-insert; the
        // map is still a valid cache, so recover it rather than propagate.
        let mut map = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = map.get(bundle_id) {
            return hit.clone();
        }
        let resolved = lookup(bundle_id);
        map.insert(bundle_id.to_owned(), resolved.clone());
        resolved
    }

    /// One Launch Services query. Every Objective-C object created here is
    /// autoreleased, hence the pool: this runs on a Tokio runtime worker
    /// thread (the mapping loops in `drain_once`/`digest_once` run after
    /// `spawn_blocking` has returned), which has no pool of its own, and
    /// without one each call would leak an `NSURL`, an `NSString` and their
    /// backing storage.
    fn lookup(bundle_id: &str) -> Option<String> {
        let c_bundle_id = CString::new(bundle_id).ok()?;
        objc::rc::autoreleasepool(|| {
            // `Class::get` rather than `class!` so a build where AppKit is
            // somehow not linked degrades to the fallback name instead of
            // panicking inside the lane.
            let ns_string = Class::get("NSString")?;
            let workspace_class = Class::get("NSWorkspace")?;
            let file_manager_class = Class::get("NSFileManager")?;

            // SAFETY: AppKit is linked via tray-icon/tao (see
            // `main.rs::bring_pid_to_front`, the msg_send! precedent). Every
            // receiver is null-checked before the next send, and the UTF-8
            // buffer is copied out before the pool drains it.
            unsafe {
                let ns_bundle_id: *mut Object =
                    msg_send![ns_string, stringWithUTF8String: c_bundle_id.as_ptr()];
                if ns_bundle_id.is_null() {
                    return None;
                }
                let workspace: *mut Object = msg_send![workspace_class, sharedWorkspace];
                if workspace.is_null() {
                    return None;
                }
                let url: *mut Object =
                    msg_send![workspace, URLForApplicationWithBundleIdentifier: ns_bundle_id];
                if url.is_null() {
                    return None;
                }
                let path: *mut Object = msg_send![url, path];
                if path.is_null() {
                    return None;
                }
                let file_manager: *mut Object = msg_send![file_manager_class, defaultManager];
                if file_manager.is_null() {
                    return None;
                }
                let name: *mut Object = msg_send![file_manager, displayNameAtPath: path];
                if name.is_null() {
                    return None;
                }
                let utf8: *const c_char = msg_send![name, UTF8String];
                if utf8.is_null() {
                    return None;
                }
                let name = CStr::from_ptr(utf8).to_string_lossy().into_owned();
                // `displayNameAtPath:` already hides the extension the way
                // Finder does; strip it defensively for a user who has
                // "show all filename extensions" on.
                let name = name.strip_suffix(".app").unwrap_or(&name).trim().to_owned();
                (!name.is_empty()).then_some(name)
            }
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    /// No Launch Services here; `app_name` falls back to the last segment.
    pub(super) fn display_name(_bundle_id: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app that is not installed — on any OS — must still get a readable
    /// name, and the same answer twice (the second one from the cache).
    #[test]
    fn an_uninstalled_bundle_id_falls_back_to_its_last_segment_on_every_os() {
        assert_eq!(app_name("com.example.NotInstalled"), "NotInstalled");
        assert_eq!(app_name("com.example.NotInstalled"), "NotInstalled");
        assert_eq!(last_segment("nodots"), "nodots");
        assert_eq!(last_segment("trailing."), "trailing.");
        assert_eq!(last_segment(""), "");
    }

    /// Finder exists on every Mac, so its display name is the one Launch
    /// Services answer a test can rely on. If this ever returns `finder`
    /// (the fallback), the objc path is broken, not Finder.
    #[cfg(target_os = "macos")]
    #[test]
    fn finder_resolves_to_its_display_name_through_launch_services() {
        assert_eq!(app_name("com.apple.finder"), "Finder");
    }
}
