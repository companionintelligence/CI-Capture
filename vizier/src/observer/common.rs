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
    UserInfo, WakeObservation, WindowInfo,
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
        let mut events = Vec::new();

        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                if let Ok(event) = msg {
                    events.extend(map_notify_event(event));
                }
            }
        }

        if !self.seen_first_snapshot {
            self.seen_first_snapshot = true;
            return Vec::new();
        }

        events
    }
}

impl Observer for BaselineObserver {
    fn snapshot(&mut self) -> Result<Observation> {
        let ts = current_ts();
        let mut windows = Vec::new();

        if let Ok(shell) = env::var("SHELL") {
            windows.push(WindowInfo {
                id: "local-shell".to_string(),
                title: env::var("TERM").unwrap_or_else(|_| "Terminal".to_string()),
                app: env::var("TERM_PROGRAM").unwrap_or_else(|_| "Terminal".to_string()),
                pid: std::process::id(),
                bounds: Bounds {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                workspace: 0,
                is_minimized: false,
                is_fullscreen: false,
            });

            let focus = windows.first().cloned();
            let terminal_ctx = current_terminal_context(Some(shell));

            return Ok(Observation {
                schema_version: 1,
                ts,
                monotonic_ms: self.started_at.elapsed().as_millis() as u64,
                idle_ms: 0,
                focus,
                windows,
                cursor: Point { x: 0, y: 0 },
                displays: vec![DisplayInfo {
                    id: 0,
                    bounds: Bounds {
                        x: 0,
                        y: 0,
                        w: 0,
                        h: 0,
                    },
                    is_primary: true,
                    scale_factor: 1.0,
                }],
                terminal_ctx,
                net_connections: collect_active_connections(self.all_connections),
                fs_events: self.collect_fs_events(),
            });
        }

        Ok(Observation {
            schema_version: 1,
            ts,
            monotonic_ms: self.started_at.elapsed().as_millis() as u64,
            idle_ms: 0,
            focus: None,
            windows,
            cursor: Point { x: 0, y: 0 },
            displays: Vec::new(),
            terminal_ctx: None,
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

fn map_notify_event(event: Event) -> Vec<FSEvent> {
    let kind = match event.kind {
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
    .to_string();

    let ts = current_ts();
    let now = SystemTime::now();

    event
        .paths
        .into_iter()
        .map(|path| FSEvent {
            path: path.display().to_string(),
            kind: kind.clone(),
            ts,
            file_activity: file_activity_for_path(&path, now),
        })
        .collect()
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
