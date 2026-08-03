//! Shared eframe setup for helper windows (Settings / Setup / Upload).
//!
//! The packaged `.app` sets `LSUIElement`, so every process of that bundle is
//! an accessory by default. Accessory apps can create windows, but macOS will
//! not reliably bring them forward when spawned from the tray. Force Regular
//! activation so Open Settings actually surfaces the window.

use eframe::NativeOptions;

/// Make this helper process a normal foreground app (macOS) so its window can
/// be activated and appears in the Dock while open.
pub fn configure_helper_window(options: &mut NativeOptions) {
    #[cfg(target_os = "macos")]
    {
        options.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            builder.with_activation_policy(ActivationPolicy::Regular);
        }));
    }
    let _ = options;
}
