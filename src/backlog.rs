//! Local unacked capture data: screenshots, file edits, activity events.
//!
//! Shared by the Settings warning and the tray status so both surfaces
//! describe the same pile.

use std::time::SystemTime;

use crate::capture_store::{self, ScreenshotBacklog};
use crate::config::AppConfig;
use crate::queue::rusqlite_impl::Queue;

#[derive(Debug, Clone, Default)]
pub struct LocalBacklog {
    pub screenshots: ScreenshotBacklog,
    pub file_events: u64,
    pub activity: ScreenshotBacklog,
}

impl LocalBacklog {
    pub fn waiting_count(&self) -> u64 {
        self.screenshots.count as u64 + self.file_events + self.activity.count as u64
    }

    pub fn is_empty(&self) -> bool {
        self.waiting_count() == 0
    }
}

/// Scan the three durable stores. Missing dirs/files are empty, not errors.
pub fn collect(config: &AppConfig) -> LocalBacklog {
    let now = SystemTime::now();
    let screenshots = capture_store::summarize(&config.capture.storage_dir, now);
    let activity = capture_store::summarize_activity(
        &capture_store::activity_dir(&config.capture.storage_dir),
        now,
    );
    let file_events = crate::config::queue_db_path()
        .ok()
        .filter(|p| p.exists())
        .and_then(|p| Queue::open(p).ok())
        .and_then(|q| q.depth().ok())
        .unwrap_or(0);
    LocalBacklog {
        screenshots,
        file_events,
        activity,
    }
}

/// Warn when anything is sitting locally and Memory is down, or when a
/// screenshot/activity file is old enough that the live upload should
/// already have deleted it.
pub fn should_warn(backlog: &LocalBacklog, disconnected: bool) -> bool {
    if backlog.is_empty() {
        return false;
    }
    disconnected || backlog.screenshots.is_stuck() || backlog.activity.is_stuck()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn waiting_count_sums_all_lanes() {
        let backlog = LocalBacklog {
            screenshots: ScreenshotBacklog {
                count: 2,
                bytes: 10,
                oldest: None,
            },
            file_events: 3,
            activity: ScreenshotBacklog {
                count: 1,
                bytes: 4,
                oldest: None,
            },
        };
        assert_eq!(backlog.waiting_count(), 6);
        assert!(should_warn(&backlog, true));
        assert!(!should_warn(&backlog, false));
    }

    #[test]
    fn stuck_activity_warns_while_connected() {
        let backlog = LocalBacklog {
            activity: ScreenshotBacklog {
                count: 1,
                bytes: 8,
                oldest: Some(Duration::from_secs(10 * 60)),
            },
            ..LocalBacklog::default()
        };
        assert!(should_warn(&backlog, false));
    }
}
