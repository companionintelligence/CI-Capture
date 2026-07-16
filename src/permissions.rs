//! OS capture-permission status + request aids for the onboarding wizard.
//!
//! The applet's capture lanes silently produce nothing without the right OS
//! grants — on macOS a missing **Screen Recording** grant yields wallpaper-only
//! frames and a missing **Accessibility** grant blanks the activity lane. This
//! module surfaces the live grant state and provides one-click aids: trigger
//! the system prompt where the OS allows it, and deep-link to the exact
//! System Settings pane otherwise.
//!
//! Only macOS gates screen/AX capture behind TCC; on Windows and Linux these
//! report [`PermissionStatus::NotRequired`] so the wizard can skip the step.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatus {
    /// Confirmed granted.
    Granted,
    /// Confirmed missing — capture will silently fail until the user grants it.
    Denied,
    /// This OS does not gate the capability behind a permission. (Constructed
    /// only in the non-macOS backend; on macOS every check is Granted/Denied.)
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    NotRequired,
}

impl PermissionStatus {
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Granted | Self::NotRequired)
    }

    /// A short label + traffic-light glyph for the UI.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Granted => "✅ Granted",
            Self::Denied => "⛔ Not granted",
            Self::NotRequired => "— Not required on this OS",
        }
    }
}

impl fmt::Display for PermissionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.badge())
    }
}

/// The two capture-gating permissions the applet cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// Screen Recording — required for the screenshot lane.
    pub screen_recording: PermissionStatus,
    /// Accessibility — required for the richest vizier activity signal.
    pub accessibility: PermissionStatus,
}

impl Permissions {
    /// True when nothing is blocking capture (all required grants present).
    pub fn all_ok(self) -> bool {
        self.screen_recording.is_ok() && self.accessibility.is_ok()
    }

    /// True when at least one required grant is missing.
    pub fn needs_attention(self) -> bool {
        self.screen_recording == PermissionStatus::Denied
            || self.accessibility == PermissionStatus::Denied
    }
}

/// Read the current permission state (non-prompting).
pub fn status() -> Permissions {
    Permissions {
        screen_recording: screen_recording_status(),
        accessibility: accessibility_status(),
    }
}

// ── macOS ───────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod imp {
    use super::PermissionStatus;

    // Framework functions. CoreGraphics gates Screen Recording; the
    // Preflight/Request pair is the modern (10.15+) API — Preflight reads the
    // grant without prompting, Request triggers the system prompt once.
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    // ApplicationServices exposes the Accessibility (AX) trust check.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }

    pub fn screen_recording_status() -> PermissionStatus {
        // SAFETY: parameterless Apple C ABI call returning a C bool.
        if unsafe { CGPreflightScreenCaptureAccess() } {
            PermissionStatus::Granted
        } else {
            PermissionStatus::Denied
        }
    }

    pub fn accessibility_status() -> PermissionStatus {
        // SAFETY: parameterless Apple C ABI call returning a Boolean.
        if unsafe { AXIsProcessTrusted() } {
            PermissionStatus::Granted
        } else {
            PermissionStatus::Denied
        }
    }

    /// Trigger the OS Screen Recording prompt (first call) and return the
    /// resulting grant state. Subsequent calls just report current state.
    pub fn request_screen_recording() -> PermissionStatus {
        // SAFETY: parameterless Apple C ABI call; may present a system dialog.
        if unsafe { CGRequestScreenCaptureAccess() } {
            PermissionStatus::Granted
        } else {
            PermissionStatus::Denied
        }
    }

    // System Settings deep links (stable `x-apple.systempreferences:` scheme).
    pub const SCREEN_RECORDING_PANE: &str =
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
    pub const ACCESSIBILITY_PANE: &str =
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
}

// ── non-macOS: nothing gates screen/AX capture behind a grant ───────────────

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::PermissionStatus;

    pub fn screen_recording_status() -> PermissionStatus {
        PermissionStatus::NotRequired
    }
    pub fn accessibility_status() -> PermissionStatus {
        PermissionStatus::NotRequired
    }
    pub fn request_screen_recording() -> PermissionStatus {
        PermissionStatus::NotRequired
    }
    pub const SCREEN_RECORDING_PANE: &str = "";
    pub const ACCESSIBILITY_PANE: &str = "";
}

pub fn screen_recording_status() -> PermissionStatus {
    imp::screen_recording_status()
}
pub fn accessibility_status() -> PermissionStatus {
    imp::accessibility_status()
}

/// Ask the OS to prompt for Screen Recording (macOS); returns the resulting
/// status. No-op returning `NotRequired` off macOS.
pub fn request_screen_recording() -> PermissionStatus {
    imp::request_screen_recording()
}

/// The System Settings pane URL for Screen Recording (empty off macOS).
pub fn screen_recording_settings_url() -> &'static str {
    imp::SCREEN_RECORDING_PANE
}

/// The System Settings pane URL for Accessibility (empty off macOS).
pub fn accessibility_settings_url() -> &'static str {
    imp::ACCESSIBILITY_PANE
}

/// Whether this OS gates capture behind permissions at all (drives whether the
/// onboarding wizard shows the permissions step).
pub fn permissions_relevant() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_readable_without_panicking() {
        // The real point: calling into the framework FFI (macOS) or the stubs
        // (elsewhere) is safe and returns a coherent struct.
        let p = status();
        assert_eq!(p.screen_recording.is_ok(), p.screen_recording != PermissionStatus::Denied);
        // all_ok and needs_attention are never both true.
        assert!(!(p.all_ok() && p.needs_attention()));
    }

    #[test]
    fn ok_and_badges() {
        assert!(PermissionStatus::Granted.is_ok());
        assert!(PermissionStatus::NotRequired.is_ok());
        assert!(!PermissionStatus::Denied.is_ok());
        assert!(PermissionStatus::Granted.badge().contains("Granted"));
        assert!(PermissionStatus::Denied.badge().contains("Not granted"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_requires_nothing() {
        assert!(!permissions_relevant());
        let p = status();
        assert_eq!(p.screen_recording, PermissionStatus::NotRequired);
        assert_eq!(p.accessibility, PermissionStatus::NotRequired);
        assert!(p.all_ok() && !p.needs_attention());
        assert!(screen_recording_settings_url().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_gates_capture_and_has_settings_links() {
        assert!(permissions_relevant());
        assert!(screen_recording_settings_url().starts_with("x-apple.systempreferences:"));
        assert!(accessibility_settings_url().contains("Privacy_Accessibility"));
        // Status resolves to a definite Granted/Denied (never NotRequired) on macOS.
        assert_ne!(accessibility_status(), PermissionStatus::NotRequired);
        assert_ne!(screen_recording_status(), PermissionStatus::NotRequired);
    }
}
