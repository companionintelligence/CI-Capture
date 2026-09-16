use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use chrono::Local;
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use sysinfo::{Disks, System};
use walkdir::WalkDir;

use crate::observation::{
    Bounds, DateTimeInfo, DisplayInfo, FSEvent, FileActivityInfo, FilesystemInfo, GpuInfo,
    HomeTreeEntry, InstalledApp, MachineInfo, MountInfo, NetworkIdentity, Observation, Point,
    RecentActivity, RecentFileInfo, ResourceInfo, RunningProcessInfo, SessionInfo, TerminalCtx,
    UserInfo, WakeObservation,
};
use crate::observer::{Observer, ObserverConfig, WakeConfig, Waker};
use crate::util::net::{collect_active_connections, collect_listening_ports};

pub struct BaselineObserver {
    started_at: Instant,
    all_connections: bool,
    rx: Option<Receiver<notify::Result<Event>>>,
    _watcher: Option<RecommendedWatcher>,
    seen_first_snapshot: bool,
}

impl BaselineObserver {
    pub fn new(config: ObserverConfig) -> Self {
        let watch_target = config.watch_path.or_else(dirs::home_dir);

        let (watcher, rx) = match watch_target {
            Some(path) => setup_watcher(&path),
            None => (None, None),
        };

        Self {
            started_at: Instant::now(),
            all_connections: config.all_connections,
            rx,
            _watcher: watcher,
            seen_first_snapshot: false,
        }
    }

    fn collect_fs_events(&mut self) -> Vec<FSEvent> {
        // Always drain the watcher channel fully so it can't grow unbounded,
        // but keep only high-signal events: the watch root is broad (the home
        // dir by default), so without filtering + a cap a busy filesystem
        // (node_modules, caches, build output, browser profiles) floods a
        // single snapshot into the megabytes. Snapshots are meant to be tiny.
        let mut raw: Vec<(String, PathBuf)> = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                if let Ok(event) = msg {
                    let kind = notify_event_kind(&event);
                    for path in event.paths {
                        if !is_noise_fs_path(&path) {
                            raw.push((kind.clone(), path));
                        }
                    }
                }
            }
        }

        if !self.seen_first_snapshot {
            self.seen_first_snapshot = true;
            return Vec::new();
        }

        // Collapse repeat events for the same path (editors emit several per
        // save), keeping the most recent, then cap to the newest MAX_FS_EVENTS.
        let mut seen = std::collections::HashSet::new();
        let mut deduped: Vec<(String, PathBuf)> = Vec::with_capacity(raw.len());
        for (kind, path) in raw.into_iter().rev() {
            if seen.insert(path.clone()) {
                deduped.push((kind, path));
            }
        }
        deduped.reverse();
        if deduped.len() > MAX_FS_EVENTS {
            let drop_to = deduped.len() - MAX_FS_EVENTS;
            deduped.drain(0..drop_to);
        }

        // Only now stat the survivors — never the thousands we just dropped.
        let now = SystemTime::now();
        let ts = current_ts();
        deduped
            .into_iter()
            .map(|(kind, path)| FSEvent {
                path: path.display().to_string(),
                kind,
                ts,
                file_activity: file_activity_for_path(&path, now),
            })
            .collect()
    }
}

/// Upper bound on filesystem events carried in one snapshot. Keeps perception
/// payloads small (snapshots are meant to be ~kilobytes, not megabytes); excess
/// churn is sampled down to the most recent events.
const MAX_FS_EVENTS: usize = 64;

/// High-churn / low-signal paths excluded from perception. These are package,
/// cache, build, and VCS internals that would otherwise drown a snapshot.
fn is_noise_fs_path(path: &Path) -> bool {
    // Directory names (lowercased) that mark a high-churn / low-signal subtree.
    // Matched per path component so both the directory itself and anything
    // under it are excluded (a substring like "/node_modules/" would miss the
    // directory's own create event, which has no trailing slash).
    const NOISE_DIRS: &[&str] = &[
        ".git",
        "node_modules",
        ".cache",
        "caches",
        "library",
        ".trash",
        "target",
        "dist",
        "build",
        ".next",
        ".gradle",
        ".venv",
        "__pycache__",
        ".npm",
        ".cargo",
        ".rustup",
        "deriveddata",
        ".idea",
        ".vscode",
    ];
    const NOISE_SUFFIXES: &[&str] = &[".ds_store", ".tmp", ".swp", ".swx", "~", ".lock"];

    let has_noise_component = path.components().any(|c| {
        let part = c.as_os_str().to_string_lossy().to_lowercase();
        NOISE_DIRS.contains(&part.as_str())
    });
    if has_noise_component {
        return true;
    }

    let s = path.to_string_lossy().to_lowercase();
    NOISE_SUFFIXES.iter().any(|suf| s.ends_with(suf))
}

fn notify_event_kind(event: &Event) -> String {
    match event.kind {
        EventKind::Create(_) => "Create",
        EventKind::Remove(_) => "Delete",
        EventKind::Modify(modify_kind) => {
            if matches!(modify_kind, notify::event::ModifyKind::Name(_)) {
                "Rename"
            } else {
                "Modify"
            }
        }
        _ => "Modify",
    }
    .to_string()
}

impl Observer for BaselineObserver {
    /// The portable snapshot: filesystem deltas, network connections, and the
    /// host process's shell/cwd. It deliberately reports **no focus and no
    /// windows** — the baseline has no way to see the desktop, and the native
    /// observers (`macos.rs`, Hyprland in `linux.rs`) layer real windows on top
    /// only when they actually find some.
    ///
    /// WHY: an earlier revision synthesised a `local-shell` window from
    /// `$SHELL`/`$TERM` with zero bounds and reported it as the focused window.
    /// On Windows and every non-Hyprland Linux desktop that fabricated window
    /// was the *only* focus Memory ever saw, so the activity timeline showed a
    /// fake terminal all day. Wrong data is worse than no data: an empty
    /// `windows` list lets the applet skip the tick entirely (see
    /// `capture_vizier` in the applet's worker) instead of shipping fiction.
    fn snapshot(&mut self) -> Result<Observation> {
        let ts = current_ts();
        let shell = env::var("SHELL").ok();

        // Display placeholder: a single zero-sized primary display when a
        // shell environment is present, otherwise nothing. Kept as-is so the
        // native observers' "replace displays only when non-empty" enrichment
        // behaves exactly as before; the placeholder is never used as focus.
        let displays = if shell.is_some() {
            vec![DisplayInfo {
                id: 0,
                bounds: Bounds {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                is_primary: true,
                scale_factor: 1.0,
            }]
        } else {
            Vec::new()
        };

        // The shell/cwd context describes the observing process itself, which
        // is genuine information about the host; it is not a window and must
        // not be promoted to one.
        let terminal_ctx = shell.and_then(|shell| current_terminal_context(Some(shell)));

        Ok(Observation {
            schema_version: 1,
            ts,
            monotonic_ms: self.started_at.elapsed().as_millis() as u64,
            idle_ms: 0,
            focus: None,
            windows: Vec::new(),
            cursor: Point { x: 0, y: 0 },
            displays,
            terminal_ctx,
            net_connections: collect_active_connections(self.all_connections),
            fs_events: self.collect_fs_events(),
        })
    }
}

pub struct BaselineWaker {
    config: WakeConfig,
}

impl BaselineWaker {
    pub fn new(config: WakeConfig) -> Self {
        Self { config }
    }
}

impl Waker for BaselineWaker {
    fn wake(&self) -> Result<WakeObservation> {
        let ts = current_ts();
        let now = Local::now();
        let mut system = System::new_all();
        system.refresh_all();

        let hostname = System::host_name().unwrap_or_else(|| "unknown".to_string());
        let home_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));

        let local_ips = local_ips();
        let (vpn_active, vpn_interface) = detect_vpn_interface();
        let uptime_seconds = system_uptime_seconds(ts);

        let wake = WakeObservation {
            schema_version: 1,
            ts,
            machine: MachineInfo {
                hostname: hostname.clone(),
                os: System::name().unwrap_or_else(|| env::consts::OS.to_string()),
                os_version: System::os_version().unwrap_or_else(|| "unknown".to_string()),
                kernel: System::kernel_version().unwrap_or_else(|| "unknown".to_string()),
                arch: env::consts::ARCH.to_string(),
                is_vm: false,
                is_container: Path::new("/.dockerenv").exists()
                    || Path::new("/run/.containerenv").exists(),
                hypervisor: None,
                chassis: "Unknown".to_string(),
            },
            user: UserInfo {
                username: whoami::username(),
                full_name: whoami::realname(),
                home_dir: home_dir.display().to_string(),
                shell: env::var("SHELL").unwrap_or_else(|_| "unknown".to_string()),
                uid: current_uid(),
                groups: Vec::new(),
            },
            datetime: DateTimeInfo {
                ts,
                iso: now.to_rfc3339(),
                timezone: now.offset().to_string(),
                utc_offset_seconds: now.offset().local_minus_utc(),
                uptime_seconds,
                login_ts: ts - uptime_seconds as f64,
            },
            filesystem: FilesystemInfo {
                home_tree: build_home_tree(&home_dir),
                recent_files: recent_files(&home_dir),
                mounts: mounts(),
            },
            installed_apps: installed_apps(),
            network_identity: NetworkIdentity {
                local_ips,
                public_ip: if self.config.no_public_ip {
                    None
                } else {
                    fetch_public_ip()
                },
                vpn_active,
                vpn_interface,
                default_gateway: None,
                dns_servers: dns_servers(),
                hostname_fqdn: Some(hostname),
            },
            listening_ports: collect_listening_ports(),
            resources: ResourceInfo {
                cpu_cores: std::thread::available_parallelism()
                    .map(|x| x.get() as u32)
                    .unwrap_or(1),
                cpu_model: system
                    .cpus()
                    .first()
                    .map(|cpu| cpu.brand().to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                ram_total_gb: bytes_to_gb(system.total_memory()),
                ram_free_gb: bytes_to_gb(system.available_memory()),
                gpus: vec![GpuInfo {
                    name: "unknown".to_string(),
                    vram_gb: None,
                    driver: "unknown".to_string(),
                }],
            },
            recent_activity: RecentActivity {
                shell_history: shell_history(20),
                running_since_boot: Vec::<RunningProcessInfo>::new(),
            },
            other_sessions: Vec::<SessionInfo>::new(),
        };

        Ok(wake)
    }
}

fn setup_watcher(
    path: &Path,
) -> (
    Option<RecommendedWatcher>,
    Option<Receiver<notify::Result<Event>>>,
) {
    let (tx, rx) = mpsc::channel();

    let watcher_result = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        NotifyConfig::default(),
    );

    let mut watcher = match watcher_result {
        Ok(watcher) => watcher,
        Err(_) => return (None, None),
    };

    if watcher.watch(path, RecursiveMode::Recursive).is_err() {
        return (None, None);
    }

    (Some(watcher), Some(rx))
}

fn current_terminal_context(shell: Option<String>) -> Option<TerminalCtx> {
    let cwd = env::current_dir().ok()?;

    Some(TerminalCtx {
        cwd: cwd.display().to_string(),
        shell: shell.unwrap_or_else(|| "unknown".to_string()),
    })
}

fn current_ts() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn build_home_tree(home: &Path) -> Vec<HomeTreeEntry> {
    let mut entries = Vec::new();

    let read_dir = match fs::read_dir(home) {
        Ok(read_dir) => read_dir,
        Err(_) => return entries,
    };

    for entry in read_dir.flatten().take(20) {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            let child_names: Vec<String> = fs::read_dir(&path)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|x| x.file_name().into_string().ok())
                .take(21)
                .collect();

            let (children, entry_count) = if child_names.len() <= 20 {
                (Some(child_names), None)
            } else {
                (None, Some(child_names.len()))
            };

            entries.push(HomeTreeEntry {
                path: tilde_path(home, &path),
                kind: "dir".to_string(),
                children,
                entry_count,
            });
        }
    }

    entries
}

fn recent_files(home: &Path) -> Vec<RecentFileInfo> {
    let now = SystemTime::now();
    let mut files = Vec::new();

    for entry in WalkDir::new(home)
        .max_depth(5)
        .into_iter()
        .filter_map(|x| x.ok())
        .filter(|x| x.file_type().is_file())
    {
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        let Some(activity) = file_activity_from_metadata(&metadata, now) else {
            continue;
        };

        files.push((
            activity.freshest_ago_s,
            entry.path().display().to_string(),
            activity,
        ));
    }

    files.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));

    files
        .into_iter()
        .take(5)
        .map(|(_, path, activity)| RecentFileInfo { path, activity })
        .collect()
}

fn file_activity_for_path(path: &Path, now: SystemTime) -> Option<FileActivityInfo> {
    let metadata = fs::metadata(path).ok()?;
    file_activity_from_metadata(&metadata, now)
}

fn file_activity_from_metadata(
    metadata: &fs::Metadata,
    now: SystemTime,
) -> Option<FileActivityInfo> {
    let created_ago_s = metadata
        .created()
        .ok()
        .and_then(|ts| age_in_seconds(now, ts));
    let accessed_ago_s = metadata
        .accessed()
        .ok()
        .and_then(|ts| age_in_seconds(now, ts));
    let modified_ago_s = metadata
        .modified()
        .ok()
        .and_then(|ts| age_in_seconds(now, ts));

    let mut freshest: Option<(&str, u64)> = None;

    for (kind, age) in [
        ("created", created_ago_s),
        ("accessed", accessed_ago_s),
        ("modified", modified_ago_s),
    ] {
        let Some(age) = age else {
            continue;
        };

        if freshest.map(|(_, best_age)| age < best_age).unwrap_or(true) {
            freshest = Some((kind, age));
        }
    }

    let (freshest_kind, freshest_ago_s) = freshest?;

    Some(FileActivityInfo {
        freshest_kind: freshest_kind.to_string(),
        freshest_ago_s,
        created_ago_s,
        accessed_ago_s,
        modified_ago_s,
    })
}

fn age_in_seconds(now: SystemTime, ts: SystemTime) -> Option<u64> {
    now.duration_since(ts)
        .ok()
        .map(|duration| duration.as_secs())
}

fn mounts() -> Vec<MountInfo> {
    let disks = Disks::new_with_refreshed_list();

    disks
        .list()
        .iter()
        .map(|disk| MountInfo {
            path: disk.mount_point().display().to_string(),
            fs_type: disk.file_system().to_string_lossy().to_string(),
            total_gb: bytes_to_gb(disk.total_space()),
            free_gb: bytes_to_gb(disk.available_space()),
        })
        .collect()
}

fn installed_apps() -> Vec<InstalledApp> {
    let mut apps = Vec::new();

    let catalog = [
        ("Visual Studio Code", "code", "ide"),
        ("Firefox", "firefox", "browser"),
        ("Google Chrome", "google-chrome", "browser"),
        ("Alacritty", "alacritty", "terminal"),
        ("WezTerm", "wezterm", "terminal"),
        ("Docker", "docker", "infra"),
        ("Python", "python3", "runtime"),
        ("Node", "node", "runtime"),
        ("Git", "git", "other"),
    ];

    for (name, id, kind) in catalog {
        if binary_in_path(id) || app_bundle_exists(name) {
            let version = if id == "python3" {
                command_version("python3", "--version")
            } else {
                None
            };

            apps.push(InstalledApp {
                name: name.to_string(),
                id: id.to_string(),
                kind: kind.to_string(),
                version,
            });
        }
    }

    apps
}

fn local_ips() -> Vec<String> {
    let mut ips = Vec::new();

    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() {
                continue;
            }
            ips.push(iface.ip().to_string());
        }
    }

    ips.sort();
    ips.dedup();
    ips
}

fn detect_vpn_interface() -> (bool, Option<String>) {
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            let name = iface.name;
            if name.starts_with("tun") || name.starts_with("wg") || name.starts_with("utun") {
                return (true, Some(name));
            }
        }
    }

    (false, None)
}

fn fetch_public_ip() -> Option<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(500))
        .timeout_read(Duration::from_millis(500))
        .timeout_write(Duration::from_millis(500))
        .build();

    let response = agent.get("https://api.ipify.org").call().ok()?;
    let body = response.into_string().ok()?;
    let ip = body.trim();

    if ip.is_empty() {
        None
    } else {
        Some(ip.to_string())
    }
}

fn dns_servers() -> Vec<String> {
    let text = fs::read_to_string("/etc/resolv.conf").unwrap_or_default();

    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("nameserver") {
                line.split_whitespace().nth(1).map(|x| x.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn shell_history(max_items: usize) -> Vec<String> {
    let home = match dirs::home_dir() {
        Some(home) => home,
        None => return Vec::new(),
    };

    let paths = [home.join(".zsh_history"), home.join(".bash_history")];

    for path in paths {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };

        let lines: Vec<String> = content
            .lines()
            .map(|line| {
                if let Some((_, command)) = line.split_once(';') {
                    command.trim().to_string()
                } else {
                    line.trim().to_string()
                }
            })
            .filter(|line| !line.is_empty())
            .collect();

        let start = lines.len().saturating_sub(max_items);
        return lines[start..].to_vec();
    }

    Vec::new()
}

fn bytes_to_gb(bytes: u64) -> f64 {
    let gb = bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    (gb * 100.0).round() / 100.0
}

fn tilde_path(home: &Path, path: &Path) -> String {
    if let Ok(suffix) = path.strip_prefix(home) {
        if suffix.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", suffix.display())
        }
    } else {
        path.display().to_string()
    }
}

fn binary_in_path(binary: &str) -> bool {
    let path_var = match env::var_os("PATH") {
        Some(path_var) => path_var,
        None => return false,
    };

    env::split_paths(&path_var).any(|dir| dir.join(binary).exists())
}

fn app_bundle_exists(name: &str) -> bool {
    Path::new("/Applications")
        .join(format!("{name}.app"))
        .exists()
}

fn command_version(binary: &str, arg: &str) -> Option<String> {
    let output = std::process::Command::new(binary).arg(arg).output().ok()?;
    let text = String::from_utf8(output.stdout)
        .ok()
        .or_else(|| String::from_utf8(output.stderr).ok())?;

    let line = text.lines().next()?.trim().to_string();
    if line.is_empty() { None } else { Some(line) }
}

fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: libc call has no memory safety preconditions and returns current effective uid.
        unsafe { libc::geteuid() }
    }

    #[cfg(not(unix))]
    {
        0
    }
}

fn system_uptime_seconds(now_ts: f64) -> u64 {
    let boot_time = System::boot_time() as f64;
    if boot_time > 0.0 && now_ts > boot_time {
        (now_ts - boot_time).max(0.0) as u64
    } else {
        let raw = System::uptime();
        let five_years = 5 * 365 * 24 * 60 * 60;
        if raw > now_ts as u64 || raw > five_years {
            0
        } else {
            raw
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> BaselineObserver {
        BaselineObserver::new(ObserverConfig {
            watch_path: Some(std::env::temp_dir()),
            all_connections: false,
        })
    }

    /// The baseline has no desktop access, so it must not invent one: focus is
    /// `None` and `windows` is empty regardless of `$SHELL`/`$TERM`. This is
    /// what the applet's "empty observation" skip relies on for Windows and
    /// non-Hyprland Linux, and it runs identically on all three CI OSes.
    #[test]
    fn baseline_snapshot_reports_no_focus_and_no_windows() {
        let mut observer = baseline();
        let snapshot = observer.snapshot().expect("snapshot should succeed");

        assert!(
            snapshot.focus.is_none(),
            "baseline must not fabricate focus"
        );
        assert!(
            snapshot.windows.is_empty(),
            "baseline must not fabricate windows (the old synthetic local-shell entry): {:?}",
            snapshot.windows
        );
    }

    /// Consecutive snapshots stay honest too — the first tick primes the
    /// filesystem watcher and later ticks must not start reporting a window.
    #[test]
    fn baseline_snapshot_stays_windowless_across_ticks() {
        let mut observer = baseline();
        for _ in 0..3 {
            let snapshot = observer.snapshot().expect("snapshot should succeed");
            assert!(snapshot.focus.is_none());
            assert!(snapshot.windows.is_empty());
            assert_eq!(snapshot.schema_version, 1);
        }
    }

    /// The display placeholder and terminal context are the only
    /// environment-derived fields left; when `$SHELL` is set both are present,
    /// and the placeholder display is zero-sized so nothing downstream can
    /// mistake it for real geometry.
    #[test]
    fn baseline_display_placeholder_is_zero_sized_and_follows_shell_env() {
        let mut observer = baseline();
        let snapshot = observer.snapshot().expect("snapshot should succeed");

        if env::var("SHELL").is_ok() {
            assert_eq!(snapshot.displays.len(), 1);
            let display = &snapshot.displays[0];
            assert!(display.is_primary);
            assert_eq!((display.bounds.w, display.bounds.h), (0, 0));
            assert!(snapshot.terminal_ctx.is_some());
        } else {
            assert!(snapshot.displays.is_empty());
            assert!(snapshot.terminal_ctx.is_none());
        }
    }
}
