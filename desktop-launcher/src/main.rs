use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;

use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

const WINDOW_LABEL: &str = "main";
const MONITOR_BIN_STEM: &str = "CI-Applet";

#[derive(Default)]
struct LauncherState {
    monitor_process: Mutex<Option<Child>>,
}

fn main() {
    tauri::Builder::default()
        .manage(LauncherState::default())
        .setup(|app| {
            let show = MenuItem::with_id(app, "show", "Show Launcher", true, None::<&str>)?;
            let launch = MenuItem::with_id(app, "launch", "Launch Monitor", true, None::<&str>)?;
            let settings = MenuItem::with_id(app, "settings", "Open Settings", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Launcher", true, None::<&str>)?;

            let menu = MenuBuilder::new(app)
                .item(&show)
                .item(&launch)
                .item(&settings)
                .separator()
                .item(&quit)
                .build()?;

            let icon = Image::from_bytes(include_bytes!("../icons/icon-32.png"))?;

            TrayIconBuilder::with_id("ci-applet-launcher")
                .icon(icon)
                .tooltip("CI-Applet Launcher")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| handle_menu_event(app, event.id.as_ref()))
                .on_tray_icon_event(|tray, event| {
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        toggle_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running CI-Applet launcher");
}

fn handle_menu_event(app: &AppHandle, event_id: &str) {
    match event_id {
        "show" => show_main_window(app),
        "launch" => {
            if let Err(err) = launch_monitor(app) {
                eprintln!("failed to launch monitor: {err}");
                show_main_window(app);
            }
        }
        "settings" => {
            if let Err(err) = launch_settings(app) {
                eprintln!("failed to launch settings: {err}");
                show_main_window(app);
            }
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

fn toggle_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };

    match window.is_visible() {
        Ok(true) => {
            let _ = window.hide();
        }
        Ok(false) | Err(_) => {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn launch_monitor(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<LauncherState>();
    let mut child_slot = state
        .monitor_process
        .lock()
        .map_err(|_| "launcher state poisoned".to_string())?;

    if let Some(child) = child_slot.as_mut() {
        match child.try_wait() {
            Ok(None) => return Ok(()),
            Ok(Some(_)) => {
                *child_slot = None;
            }
            Err(err) => return Err(format!("failed to inspect monitor process: {err}")),
        }
    }

    let child = Command::new(resolve_monitor_binary().ok_or_else(missing_binary_message)?)
        .spawn()
        .map_err(|err| format!("failed to start monitor binary: {err}"))?;
    *child_slot = Some(child);
    Ok(())
}

fn launch_settings(_app: &AppHandle) -> Result<(), String> {
    Command::new(resolve_monitor_binary().ok_or_else(missing_binary_message)?)
        .arg("--settings-ui")
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to start settings UI: {err}"))
}

fn resolve_monitor_binary() -> Option<PathBuf> {
    candidate_monitor_paths(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        &std::env::current_exe().ok()?,
    )
    .into_iter()
    .find(|path| path.is_file())
}

fn candidate_monitor_paths(manifest_dir: &Path, current_exe: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(path) = std::env::var_os("CI_APPLET_MONITOR_BIN").map(PathBuf::from) {
        paths.push(path);
    }

    let repo_root = manifest_dir.parent().unwrap_or(manifest_dir);
    paths.push(
        repo_root
            .join("target")
            .join("debug")
            .join(monitor_binary_name()),
    );
    paths.push(
        repo_root
            .join("target")
            .join("release")
            .join(monitor_binary_name()),
    );

    if let Some(exe_dir) = current_exe.parent() {
        paths.push(exe_dir.join(monitor_binary_name()));
        paths.push(exe_dir.join("binaries").join(sidecar_binary_name()));

        if let Some(parent) = exe_dir.parent() {
            paths.push(
                parent
                    .join("Resources")
                    .join("binaries")
                    .join(sidecar_binary_name()),
            );
            paths.push(parent.join("lib").join(monitor_binary_name()));
        }
    }

    paths
}

fn monitor_binary_name() -> String {
    if cfg!(windows) {
        format!("{MONITOR_BIN_STEM}.exe")
    } else {
        MONITOR_BIN_STEM.to_string()
    }
}

fn sidecar_binary_name() -> String {
    let target = std::env::var("TARGET").unwrap_or_else(|_| String::from("unknown-target"));
    if cfg!(windows) {
        format!("{MONITOR_BIN_STEM}-{target}.exe")
    } else {
        format!("{MONITOR_BIN_STEM}-{target}")
    }
}

fn missing_binary_message() -> String {
    format!(
        "Unable to find the CI-Applet monitor binary. Build the root crate first or set CI_APPLET_MONITOR_BIN to the full path of {}.",
        monitor_binary_name()
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{candidate_monitor_paths, monitor_binary_name, sidecar_binary_name};

    #[test]
    fn monitor_binary_name_matches_platform() {
        let binary_name = monitor_binary_name();
        if cfg!(windows) {
            assert!(binary_name.ends_with(".exe"));
        } else {
            assert_eq!(binary_name, "CI-Applet");
        }
    }

    #[test]
    fn candidate_paths_include_repo_build_outputs() {
        let manifest_dir = PathBuf::from("/repo/desktop-launcher");
        let current_exe = PathBuf::from("/repo/desktop-launcher/target/debug/desktop-launcher");

        let candidates = candidate_monitor_paths(&manifest_dir, &current_exe);

        assert!(candidates.contains(&PathBuf::from(format!(
            "/repo/target/debug/{}",
            monitor_binary_name()
        ))));
        assert!(candidates.contains(&PathBuf::from(format!(
            "/repo/target/release/{}",
            monitor_binary_name()
        ))));
        assert!(
            candidates
                .iter()
                .any(|candidate| { candidate.ends_with(PathBuf::from(sidecar_binary_name())) })
        );
    }
}
