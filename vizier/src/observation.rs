use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeObservation {
    pub schema_version: u32,
    pub ts: f64,
    pub machine: MachineInfo,
    pub user: UserInfo,
    pub datetime: DateTimeInfo,
    pub filesystem: FilesystemInfo,
    pub installed_apps: Vec<InstalledApp>,
    pub network_identity: NetworkIdentity,
    pub listening_ports: Vec<ListeningPort>,
    pub resources: ResourceInfo,
    pub recent_activity: RecentActivity,
    pub other_sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineInfo {
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub kernel: String,
    pub arch: String,
    pub is_vm: bool,
    pub is_container: bool,
    pub hypervisor: Option<String>,
    pub chassis: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub username: String,
    pub full_name: String,
    pub home_dir: String,
    pub shell: String,
    pub uid: u32,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateTimeInfo {
    pub ts: f64,
    pub iso: String,
    pub timezone: String,
    pub utc_offset_seconds: i32,
    pub uptime_seconds: u64,
    pub login_ts: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemInfo {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub home_tree: Vec<HomeTreeEntry>,
    pub recent_files: Vec<RecentFileInfo>,
    pub mounts: Vec<MountInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeTreeEntry {
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentFileInfo {
    pub path: String,
    #[serde(flatten)]
    pub activity: FileActivityInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileActivityInfo {
    pub freshest_kind: String,
    pub freshest_ago_s: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_ago_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessed_ago_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_ago_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountInfo {
    pub path: String,
    pub fs_type: String,
    pub total_gb: f64,
    pub free_gb: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledApp {
    pub name: String,
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkIdentity {
    pub local_ips: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_ip: Option<String>,
    pub vpn_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vpn_interface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_gateway: Option<String>,
    pub dns_servers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname_fqdn: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListeningPort {
    pub port: u16,
    pub proto: String,
    pub pid: u32,
    pub app: String,
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub cpu_cores: u32,
    pub cpu_model: String,
    pub ram_total_gb: f64,
    pub ram_free_gb: f64,
    pub gpus: Vec<GpuInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vram_gb: Option<f64>,
    pub driver: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentActivity {
    pub shell_history: Vec<String>,
    pub running_since_boot: Vec<RunningProcessInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningProcessInfo {
    pub pid: u32,
    pub app: String,
    pub started_ago_s: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub username: String,
    pub tty: String,
    pub from: String,
    pub login_ts: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub schema_version: u32,
    pub ts: f64,
    pub monotonic_ms: u64,
    pub idle_ms: u64,
    pub focus: Option<WindowInfo>,
    pub windows: Vec<WindowInfo>,
    pub cursor: Point,
    pub displays: Vec<DisplayInfo>,
    pub terminal_ctx: Option<TerminalCtx>,
    pub net_connections: Vec<ConnInfo>,
    pub fs_events: Vec<FSEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub app: String,
    pub pid: u32,
    pub bounds: Bounds,
    pub workspace: i32,
    pub is_minimized: bool,
    pub is_fullscreen: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: i32,
    pub bounds: Bounds,
    pub is_primary: bool,
    pub scale_factor: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalCtx {
    pub cwd: String,
    pub shell: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnInfo {
    pub proto: String,
    pub local_port: u16,
    pub remote_addr: String,
    pub remote_port: u16,
    pub pid: u32,
    pub app: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_host_count: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FSEvent {
    pub path: String,
    pub kind: String,
    pub ts: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_activity: Option<FileActivityInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl WakeObservation {
    pub fn compact(mut self) -> Self {
        self.user.groups = compact_groups(std::mem::take(&mut self.user.groups));
        self.filesystem.home_tree.clear();
        self.filesystem.recent_files =
            compact_recent_files(std::mem::take(&mut self.filesystem.recent_files));
        self.filesystem.mounts = compact_mounts(std::mem::take(&mut self.filesystem.mounts));
        self.network_identity.local_ips =
            compact_local_ips(std::mem::take(&mut self.network_identity.local_ips));
        self.listening_ports = compact_listening_ports(std::mem::take(&mut self.listening_ports));
        self.recent_activity.shell_history =
            compact_shell_history(std::mem::take(&mut self.recent_activity.shell_history));
        self.recent_activity.running_since_boot.clear();
        self.other_sessions.retain(|session| {
            let from = session.from.trim().to_ascii_lowercase();
            !(from.is_empty() || from == "local" || from == "-")
        });
        self.other_sessions.truncate(3);
        self
    }
}

impl Observation {
    pub fn compact(mut self) -> Self {
        self.net_connections = compact_net_connections(std::mem::take(&mut self.net_connections));
        self
    }
}

fn compact_groups(groups: Vec<String>) -> Vec<String> {
    let mut filtered: Vec<String> = groups
        .into_iter()
        .filter(|group| {
            !group.starts_with('_')
                && !group.starts_with("com.apple.")
                && !matches!(
                    group.as_str(),
                    "everyone" | "localaccounts" | "_appserverusr" | "_appserveradm"
                )
        })
        .collect();

    filtered.sort();
    filtered.dedup();

    if filtered.iter().any(|group| group == "admin") {
        return vec!["admin".to_string()];
    }

    filtered.truncate(2);
    filtered
}

fn compact_net_connections(connections: Vec<ConnInfo>) -> Vec<ConnInfo> {
    let mut grouped: BTreeMap<(String, u32, String, String), (ConnInfo, u32, BTreeSet<String>)> =
        BTreeMap::new();

    for mut connection in connections {
        connection.connection_count = None;
        connection.remote_host_count = None;
        let key = (
            connection.app.clone(),
            connection.pid,
            connection.proto.clone(),
            connection.state.clone(),
        );

        if let Some((_, count, remote_hosts)) = grouped.get_mut(&key) {
            *count += 1;
            remote_hosts.insert(connection.remote_addr.clone());
        } else {
            let mut remote_hosts = BTreeSet::new();
            remote_hosts.insert(connection.remote_addr.clone());
            grouped.insert(key, (connection, 1, remote_hosts));
        }
    }

    let mut compacted: Vec<ConnInfo> = grouped
        .into_values()
        .map(|(mut connection, count, remote_hosts)| {
            if count > 1 {
                connection.connection_count = Some(count);
                connection.local_port = 0;
                connection.remote_addr = "(multiple)".to_string();
                connection.remote_port = 0;
                connection.remote_host_count = Some(remote_hosts.len() as u32);
            }
            connection
        })
        .collect();

    compacted.sort_by(|left, right| {
        right
            .connection_count
            .unwrap_or(1)
            .cmp(&left.connection_count.unwrap_or(1))
            .then(left.app.cmp(&right.app))
            .then(left.pid.cmp(&right.pid))
    });

    compacted
}

fn compact_recent_files(files: Vec<RecentFileInfo>) -> Vec<RecentFileInfo> {
    let mut compacted: Vec<RecentFileInfo> = files;

    compacted.truncate(5);
    compacted
}

fn compact_mounts(mounts: Vec<MountInfo>) -> Vec<MountInfo> {
    let mut compacted: Vec<MountInfo> = mounts
        .into_iter()
        .filter(|mount| {
            mount.path == "/" || mount.path == "/home" || mount.path.starts_with("/Volumes/")
        })
        .collect();

    compacted.sort_by(|left, right| left.path.cmp(&right.path));
    compacted.dedup_by(|left, right| left.path == right.path);
    compacted.truncate(3);
    compacted
}

fn compact_local_ips(ips: Vec<String>) -> Vec<String> {
    let mut filtered: Vec<String> = ips.into_iter().filter(|ip| ip.contains('.')).collect();
    filtered.sort();
    filtered.dedup();
    filtered.truncate(2);
    filtered
}

fn compact_listening_ports(ports: Vec<ListeningPort>) -> Vec<ListeningPort> {
    let keep_ports = [11434_u16, 8080, 6379, 5432, 5173, 3030, 3000, 5000];
    let noise_apps = ["controlce", "rapportd", "ardagent", "identitys"];

    let mut filtered: Vec<ListeningPort> = ports
        .into_iter()
        .filter(|port| {
            let app = port.app.to_ascii_lowercase();
            !noise_apps.iter().any(|noise| app.starts_with(noise))
                && (keep_ports.contains(&port.port) || port.port <= 32768)
        })
        .collect();

    filtered.sort_by(|left, right| left.port.cmp(&right.port).then(left.app.cmp(&right.app)));
    filtered.dedup_by(|left, right| left.port == right.port && left.app == right.app);
    filtered.truncate(12);
    filtered
}

fn compact_shell_history(history: Vec<String>) -> Vec<String> {
    let mut filtered: Vec<String> = history
        .into_iter()
        .filter_map(|line| normalize_shell_history_line(&line))
        .collect();

    filtered.dedup();
    let start = filtered.len().saturating_sub(5);
    filtered.drain(0..start);
    filtered
}

fn normalize_shell_history_line(line: &str) -> Option<String> {
    let mut value = line.trim().to_string();
    if value.is_empty() {
        return None;
    }

    if let Some((prefix, _)) = value.split_once(";                 EC=") {
        value = prefix.trim().to_string();
    }

    if value.starts_with("PS1=") || value.starts_with("PS2=") {
        return None;
    }

    if value.contains("___BEGIN___COMMAND_DONE_MARKER___") {
        return None;
    }

    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::{ConnInfo, compact_net_connections};

    #[test]
    fn compact_net_connections_groups_duplicate_apps() {
        let compacted = compact_net_connections(vec![
            ConnInfo {
                proto: "tcp".to_string(),
                local_port: 1000,
                remote_addr: "1.1.1.1".to_string(),
                remote_port: 443,
                pid: 42,
                app: "Browser".to_string(),
                state: "ESTABLISHED".to_string(),
                connection_count: None,
                remote_host_count: None,
            },
            ConnInfo {
                proto: "tcp".to_string(),
                local_port: 1001,
                remote_addr: "1.1.1.2".to_string(),
                remote_port: 443,
                pid: 42,
                app: "Browser".to_string(),
                state: "ESTABLISHED".to_string(),
                connection_count: None,
                remote_host_count: None,
            },
            ConnInfo {
                proto: "tcp".to_string(),
                local_port: 2000,
                remote_addr: "2.2.2.2".to_string(),
                remote_port: 443,
                pid: 7,
                app: "Discord".to_string(),
                state: "ESTABLISHED".to_string(),
                connection_count: None,
                remote_host_count: None,
            },
        ]);

        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0].app, "Browser");
        assert_eq!(compacted[0].connection_count, Some(2));
        assert_eq!(compacted[0].remote_host_count, Some(2));
        assert_eq!(compacted[0].local_port, 0);
        assert_eq!(compacted[0].remote_addr, "(multiple)");
        assert_eq!(compacted[0].remote_port, 0);
        assert_eq!(compacted[1].app, "Discord");
        assert_eq!(compacted[1].connection_count, None);
        assert_eq!(compacted[1].remote_host_count, None);
    }
}
