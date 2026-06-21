use vizier::diff::create_diff_envelope;
use vizier::observer::common::{BaselineObserver, BaselineWaker};
use vizier::observer::{Observer, ObserverConfig, WakeConfig, Waker};

#[test]
fn snapshot_shape_has_required_fields() {
    let mut observer = BaselineObserver::new(ObserverConfig {
        watch_path: Some(std::env::temp_dir()),
        all_connections: false,
    });

    let snapshot = observer.snapshot().expect("snapshot should succeed");

    assert_eq!(snapshot.schema_version, 1);
    assert!(snapshot.ts > 0.0);
    assert!(snapshot.monotonic_ms <= 5_000);
}

#[test]
fn wake_respects_no_public_ip_flag() {
    let waker = BaselineWaker::new(WakeConfig { no_public_ip: true });
    let wake = waker.wake().expect("wake should succeed");

    assert_eq!(wake.schema_version, 1);
    assert!(wake.ts > 0.0);
    assert!(wake.network_identity.public_ip.is_none());
}

#[test]
fn diff_envelope_contains_patch_operations() {
    let mut observer = BaselineObserver::new(ObserverConfig {
        watch_path: Some(std::env::temp_dir()),
        all_connections: false,
    });

    let previous = observer.snapshot().expect("first snapshot should succeed");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let current = observer.snapshot().expect("second snapshot should succeed");

    let envelope = create_diff_envelope(&previous, &current).expect("diff should succeed");

    assert!(envelope.ts >= previous.ts);
    assert_eq!(envelope.monotonic_ms, current.monotonic_ms);
    assert!(!envelope.patch.0.is_empty());
}
