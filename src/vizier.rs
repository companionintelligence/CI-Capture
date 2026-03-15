pub mod observation {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct FocusWindow {
        pub app: String,
        pub title: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct TerminalContext {
        pub cwd: PathBuf,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct FileSystemEvent {
        pub path: PathBuf,
        pub kind: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct Observation {
        pub monotonic_ms: u64,
        pub focus: Option<FocusWindow>,
        pub terminal_ctx: Option<TerminalContext>,
        #[serde(default)]
        pub fs_events: Vec<FileSystemEvent>,
        pub watch_path: Option<PathBuf>,
        pub all_connections: bool,
    }
}

pub mod diff {
    use anyhow::Result;
    use serde::{Deserialize, Serialize};

    use crate::vizier::observation::Observation;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct DiffEnvelope {
        pub monotonic_ms: u64,
        pub previous_monotonic_ms: u64,
        pub changed_fields: Vec<String>,
        pub current: Observation,
    }

    pub fn create_diff_envelope(
        previous: &Observation,
        current: &Observation,
    ) -> Result<DiffEnvelope> {
        let mut changed_fields = Vec::new();
        if previous.focus != current.focus {
            changed_fields.push("focus".to_string());
        }
        if previous.terminal_ctx != current.terminal_ctx {
            changed_fields.push("terminal_ctx".to_string());
        }
        if previous.fs_events != current.fs_events {
            changed_fields.push("fs_events".to_string());
        }
        if previous.watch_path != current.watch_path {
            changed_fields.push("watch_path".to_string());
        }
        if previous.all_connections != current.all_connections {
            changed_fields.push("all_connections".to_string());
        }

        Ok(DiffEnvelope {
            monotonic_ms: current.monotonic_ms,
            previous_monotonic_ms: previous.monotonic_ms,
            changed_fields,
            current: current.clone(),
        })
    }
}

pub mod observer {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use anyhow::Result;

    use crate::vizier::observation::{FileSystemEvent, FocusWindow, Observation, TerminalContext};

    pub trait Observer: Send {
        fn snapshot(&mut self) -> Result<Observation>;
    }

    #[derive(Debug, Clone)]
    pub struct ObserverConfig {
        pub watch_path: Option<PathBuf>,
        pub all_connections: bool,
    }

    pub fn create_observer(config: ObserverConfig) -> Box<dyn Observer> {
        Box::new(LocalObserver::new(config))
    }

    struct LocalObserver {
        config: ObserverConfig,
        started_at: Arc<Instant>,
    }

    impl LocalObserver {
        fn new(config: ObserverConfig) -> Self {
            Self {
                config,
                started_at: Arc::new(Instant::now()),
            }
        }
    }

    impl Observer for LocalObserver {
        fn snapshot(&mut self) -> Result<Observation> {
            let cwd = std::env::current_dir().ok();
            let fs_events = self
                .config
                .watch_path
                .clone()
                .filter(|path| path.exists())
                .map(|path| {
                    vec![FileSystemEvent {
                        path,
                        kind: "watch_path_present".to_string(),
                    }]
                })
                .unwrap_or_default();

            Ok(Observation {
                monotonic_ms: self.started_at.elapsed().as_millis() as u64,
                focus: Some(FocusWindow {
                    app: whoami::platform().to_string(),
                    title: "CI-Applet".to_string(),
                }),
                terminal_ctx: cwd.map(|cwd| TerminalContext { cwd }),
                fs_events,
                watch_path: self.config.watch_path.clone(),
                all_connections: self.config.all_connections,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::vizier::diff::create_diff_envelope;
    use crate::vizier::observation::{Observation, TerminalContext};

    #[test]
    fn diff_envelope_tracks_changed_fields() {
        let previous = Observation {
            monotonic_ms: 10,
            focus: None,
            terminal_ctx: Some(TerminalContext {
                cwd: PathBuf::from("/tmp/old"),
            }),
            fs_events: Vec::new(),
            watch_path: None,
            all_connections: false,
        };
        let current = Observation {
            monotonic_ms: 20,
            focus: None,
            terminal_ctx: Some(TerminalContext {
                cwd: PathBuf::from("/tmp/new"),
            }),
            fs_events: Vec::new(),
            watch_path: Some(PathBuf::from("/tmp/watch")),
            all_connections: true,
        };

        let diff = create_diff_envelope(&previous, &current).expect("diff envelope");

        assert_eq!(diff.previous_monotonic_ms, 10);
        assert_eq!(diff.monotonic_ms, 20);
        assert!(diff.changed_fields.contains(&"terminal_ctx".to_string()));
        assert!(diff.changed_fields.contains(&"watch_path".to_string()));
        assert!(diff.changed_fields.contains(&"all_connections".to_string()));
    }
}
