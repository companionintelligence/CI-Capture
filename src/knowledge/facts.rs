//! Stream policy and the `KnowledgeRow` → `CaptureEvent` mapping: which
//! streams ship by default, which rows are too short or too sensitive to
//! ship, and what each stream looks like on the wire.
//!
//! Two properties of CI-Server's `POST /v1/events` shape everything here:
//!
//! - **Re-posting a `source_id` is a silent `duplicate`, never an update**,
//!   so an event's identity must be final the first time it is seen. Local
//!   Knowledge rows are written once at session end and never mutated, and
//!   `ZUUID` is unique per row, so `knowledge:<ZUUID>` is that identity.
//! - **Only `title`/`description` and an exact-case metadata allowlist
//!   (`Title`, `Artist`, `Album`, …) are search-indexed.** Titles therefore
//!   carry the human-readable fact ("Used Google Chrome for 12 min") and
//!   the media keys keep their capitalised spelling on purpose.
//!
//! Every wire timestamp goes through [`crate::server_ingest::wire_ts`]
//! (UTC, trailing `Z` — the only form the server's `z.string().datetime()`
//! accepts). `ZSECONDSFROMGMT` rides along in the payload for day
//! bucketing and is never used to build a wire timestamp.
//!
//! # Privacy rules encoded here
//!
//! - Sign-in and password-manager activity is suppressed by a code-side,
//!   **non-disableable** list ([`SENSITIVE_BUNDLE_IDS`],
//!   [`SENSITIVE_WEB_DOMAINS`]) that the user's own exclusion lists stack on
//!   top of. The same functions gate the daily digests, so a hidden app
//!   cannot be inferred from a total either.
//! - Bundle rules cannot protect `/app/webUsage` — every row there is
//!   `com.apple.Safari` — which is why the domain list exists and why that
//!   stream is opt-in.
//! - Descriptions carry no clock times (the server already has start/end),
//!   no URLs, no notification bodies, no contact identifiers — the reader
//!   never selects those columns, and this module has no field to put
//!   them in.
//! - `_SYSTEM_CENTER_:` notifications (accountsd, DiskArbitration…) are
//!   dropped: they describe daemons, not the user. So is any other
//!   `_`-prefixed pseudo-bundle: only `_WEB_CENTER_:` is understood, and an
//!   unrecognised one must not be mistaken for a real bundle id.

use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::app_names::app_name;
use super::reader::{
    DailyAppUsage, DailyUsage, KnowledgeRow, cocoa_secs_to_unix_ms, local_midnight_cocoa_secs,
};
use crate::config::KnowledgeConfig;
use crate::server_ingest::{CaptureEvent, wire_ts, wire_ts_utc};

// ── Stream names ────────────────────────────────────────────────────────────

pub const STREAM_APP_USAGE: &str = "/app/usage";
pub const STREAM_MEDIA_USAGE: &str = "/app/mediaUsage";
pub const STREAM_NOTIFICATION: &str = "/notification/usage";
pub const STREAM_BLUETOOTH: &str = "/bluetooth/isConnected";
pub const STREAM_DISPLAY: &str = "/display/isBacklit";
pub const STREAM_WEB_USAGE: &str = "/app/webUsage";
pub const STREAM_INTENTS: &str = "/app/intents";

/// Shipped unless listed in `disabled_streams`: this Mac's own local
/// activity. `/display/isBacklit` is deliberately absent — 80% of its
/// non-zero rows are sub-30 s same-state re-emissions with nothing to
/// search for (~1,000 noise events/day on the reference host); display-on
/// time still reaches the server through the daily digest.
pub const DEFAULT_STREAMS: &[&str] = &[
    STREAM_APP_USAGE,
    STREAM_MEDIA_USAGE,
    STREAM_NOTIFICATION,
    STREAM_BLUETOOTH,
];

/// Enableable via `extra_streams`. `/app/webUsage` is Safari domains (never
/// URLs); `/app/intents` is 100% synced from iPhone/iPad and so also needs
/// `include_synced_devices`; `/display/isBacklit` ships only "on" spans of
/// at least [`DISPLAY_MIN_ON_SECS`].
pub const OPT_IN_STREAMS: &[&str] = &[STREAM_WEB_USAGE, STREAM_INTENTS, STREAM_DISPLAY];

/// Every stream this lane can map. Anything else in `extra_streams` is
/// ignored by [`effective_streams`] — there is no wire shape for it.
pub const KNOWN_STREAMS: &[&str] = &[
    STREAM_APP_USAGE,
    STREAM_MEDIA_USAGE,
    STREAM_NOTIFICATION,
    STREAM_BLUETOOTH,
    STREAM_DISPLAY,
    STREAM_WEB_USAGE,
    STREAM_INTENTS,
];

/// Streams whose rows are instants, not spans: `ended_at` is sent only
/// when `end > start`, and no duration floor ever drops them.
pub const POINT_EVENT_STREAMS: &[&str] = &[STREAM_NOTIFICATION, STREAM_INTENTS];

/// Display "on" spans shorter than this are not shipped as raw events
/// (timestamps there are 4 s-quantized and 4,278 of 4,897 "on" rows on the
/// reference host are 0 s).
pub const DISPLAY_MIN_ON_SECS: u64 = 300;

// ── Suppression lists (code-side, non-disableable) ──────────────────────────

/// Lowercase bundle-id prefixes for password managers and credential
/// stores, verified against the real bundle ids those apps ship. Every
/// entry is a prefix match: a trailing `.` scopes to a vendor's apps, and
/// an entry without one also covers suffixed variants (`in.sinew.enpass`
/// matches `in.sinew.Enpass-Desktop`) — the broader reading is the safe
/// one for a suppression list. Stacked on this, `is_suppressed_bundle`
/// also drops any id containing `password` or `keepass`, which catches
/// renamed builds of the listed apps; the user's `excluded_bundle_ids`
/// covers anything else.
pub const SENSITIVE_BUNDLE_IDS: &[&str] = &[
    "com.1password.",
    "com.agilebits.",
    "com.bitwarden.",
    "com.lastpass.",
    "com.dashlane.",
    "org.keepassxc.",
    "com.keepassium.",
    "com.apple.keychainaccess",
    "com.apple.passwords",
    "com.nordpass.",
    "com.sibersystems.",
    "in.sinew.enpass",
    "com.markmcguill.strongbox",
    "me.proton.pass",
    "com.keepersecurity.",
    "com.callpod.keeper",
    "com.outercorner.secrets",
    "com.mseven.msecure",
    "com.safeincloud.",
];

/// Sign-in and vault hosts, matched as domain suffixes (so
/// `accounts.google.com` also covers any subdomain of it). Only
/// `/app/webUsage` and web-center notifications carry a domain at all.
pub const SENSITIVE_WEB_DOMAINS: &[&str] = &[
    "accounts.google.com",
    "login.microsoftonline.com",
    "login.live.com",
    "appleid.apple.com",
    "idmsa.apple.com",
    "auth0.com",
    "okta.com",
    "login.yahoo.com",
    "signin.aws.amazon.com",
    "my.1password.com",
    "vault.bitwarden.com",
    "lastpass.com",
];

// ── Policy ──────────────────────────────────────────────────────────────────

/// (`DEFAULT_STREAMS` ∪ `extra_streams`) − `disabled_streams`, deduplicated,
/// defaults first. Defaults live in code (the `DEFAULT_IGNORES` precedent)
/// so a new default reaches existing installs. Unknown names are dropped:
/// the reader would happily return their rows, but there is no wire shape
/// for them.
pub fn effective_streams(config: &KnowledgeConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(KNOWN_STREAMS.len());
    for stream in DEFAULT_STREAMS
        .iter()
        .copied()
        .chain(config.extra_streams.iter().map(String::as_str))
    {
        if !KNOWN_STREAMS.contains(&stream) {
            continue;
        }
        if config.disabled_streams.iter().any(|d| d == stream) {
            continue;
        }
        if !out.iter().any(|s| s == stream) {
            out.push(stream.to_owned());
        }
    }
    out
}

/// True when activity in this app must not be reported: a code-side
/// prefix match on [`SENSITIVE_BUNDLE_IDS`], the `password`/`keepass`
/// substring heuristic, or an exact (case-insensitive) match on the user's
/// `excluded_bundle_ids`. Shared by the raw mapping and the digests.
pub fn is_suppressed_bundle(bundle_id: &str, config: &KnowledgeConfig) -> bool {
    let lower = bundle_id.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    if SENSITIVE_BUNDLE_IDS
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }
    if lower.contains("password") || lower.contains("keepass") {
        return true;
    }
    config
        .excluded_bundle_ids
        .iter()
        .any(|excluded| excluded.trim().eq_ignore_ascii_case(&lower))
}

/// True when a web host must not be reported: a suffix match on
/// [`SENSITIVE_WEB_DOMAINS`] or on the user's `excluded_domains`. Suffix
/// means label-aligned — `okta.com` covers `acme.okta.com` but not
/// `notokta.com`.
pub fn is_suppressed_domain(domain: &str, config: &KnowledgeConfig) -> bool {
    let lower = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    SENSITIVE_WEB_DOMAINS
        .iter()
        .copied()
        .chain(config.excluded_domains.iter().map(String::as_str))
        .any(|suffix| {
            let suffix = suffix
                .trim()
                .trim_start_matches('.')
                .trim_end_matches('.')
                .to_ascii_lowercase();
            !suffix.is_empty()
                && (lower == suffix
                    || lower
                        .strip_suffix(&suffix)
                        .is_some_and(|head| head.ends_with('.')))
        })
}

/// Minimum whole-second duration for a row of `stream` to ship as a raw
/// event, or `None` when the stream is never floored (point events).
///
/// - `/app/usage`, `/app/webUsage` → `min_session_secs` (`0` still cuts
///   zero-length rows: "every non-zero session");
/// - `/app/mediaUsage`, `/bluetooth/isConnected` → 1 (zero-only cut);
/// - `/display/isBacklit` → [`DISPLAY_MIN_ON_SECS`];
/// - anything else unknown → 1, so an unmapped stream never ships instants.
pub fn min_duration_secs(stream: &str, config: &KnowledgeConfig) -> Option<u64> {
    match stream {
        s if POINT_EVENT_STREAMS.contains(&s) => None,
        STREAM_APP_USAGE | STREAM_WEB_USAGE => Some(config.min_session_secs.max(1)),
        STREAM_MEDIA_USAGE | STREAM_BLUETOOTH => Some(1),
        STREAM_DISPLAY => Some(DISPLAY_MIN_ON_SECS),
        _ => Some(1),
    }
}

/// `45 s`, `12 min`, `1 h 05 min` — the granularity a person would use in
/// a sentence. Seconds are dropped once minutes exist; minutes are
/// zero-padded once hours exist so "1 h 5 min" cannot be misread as 1:50.
pub fn humanize_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else {
        format!("{} h {:02} min", secs / 3600, (secs % 3600) / 60)
    }
}

// ── Mapping ─────────────────────────────────────────────────────────────────

/// Everything the mapping needs beyond the row itself.
#[derive(Debug, Clone, Copy)]
pub struct FactContext<'a> {
    pub config: &'a KnowledgeConfig,
    /// The applet's own identity (`config.identity`), nil when unset — the
    /// same convention as the imessage lane's payloads.
    pub device_id: Uuid,
    /// `Z_METADATA.Z_UUID`, for the `source_id` fallback on a row without
    /// a `ZUUID`.
    pub store_uuid: Option<&'a str>,
    /// Whether `/app/webUsage` is in the effective stream set. When it is
    /// not, web-center notifications are shipped without their host: the
    /// user has not opted into domain-level browsing facts, and a
    /// notification row must not become the side channel that leaks them.
    pub web_effective: bool,
}

/// Why a row was not turned into an event. Each variant is a counter in
/// the lane's poll outcome; the cursor advances past all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Below the stream's duration floor ([`min_duration_secs`]).
    Short,
    /// Suppressed by [`is_suppressed_bundle`] / [`is_suppressed_domain`].
    Sensitive,
    /// Not a user-activity fact by policy: a `_SYSTEM_CENTER_:`
    /// notification, a display-*off* span, a row missing the value that
    /// names its subject, or a stream this module has no shape for.
    System,
}

/// The wire event for one row, or the reason it does not ship. Sensitive
/// checks run before the duration floor so a suppressed app never
/// contributes to the "short" count either.
pub fn to_capture_event(row: &KnowledgeRow, ctx: &FactContext<'_>) -> Result<CaptureEvent, Skip> {
    let config = ctx.config;
    let duration_secs = row.duration_secs();
    let is_point = POINT_EVENT_STREAMS.contains(&row.stream.as_str());

    // Per-stream subject resolution and suppression, before anything is
    // formatted or looked up.
    let shape = match row.stream.as_str() {
        STREAM_APP_USAGE => {
            let bundle_id = non_empty(row.value_string.as_deref()).ok_or(Skip::System)?;
            if is_suppressed_bundle(bundle_id, config) {
                return Err(Skip::Sensitive);
            }
            floor_check(row, config, duration_secs)?;
            let app = app_name(bundle_id);
            Shape {
                kind: "knowledge_app_usage",
                title: format!("Used {app} for {}", humanize_duration(duration_secs)),
                description: Some(format!("{app} ({bundle_id})")),
                extras: json!({
                    "bundle_id": bundle_id,
                    "app_name": app,
                    "usage_trusted": row.usage_trusted.map(|v| v != 0),
                }),
            }
        }
        STREAM_MEDIA_USAGE => {
            let bundle_id = non_empty(row.value_string.as_deref()).ok_or(Skip::System)?;
            if is_suppressed_bundle(bundle_id, config) {
                return Err(Skip::Sensitive);
            }
            floor_check(row, config, duration_secs)?;
            let app = app_name(bundle_id);
            let mut title = format!(
                "Played media in {app} for {}",
                humanize_duration(duration_secs)
            );
            let media_title = non_empty(row.media_title.as_deref());
            let media_artist = non_empty(row.media_artist.as_deref());
            if let Some(t) = media_title {
                title.push_str(" — ");
                title.push_str(t);
                if let Some(a) = media_artist {
                    title.push_str(" by ");
                    title.push_str(a);
                }
            }
            let mut extras = Map::new();
            extras.insert("bundle_id".into(), json!(bundle_id));
            extras.insert("app_name".into(), json!(app));
            // Exact-case keys: CI-Server's search-indexed metadata allowlist.
            if let Some(t) = media_title {
                extras.insert("Title".into(), json!(t));
            }
            if let Some(a) = media_artist {
                extras.insert("Artist".into(), json!(a));
            }
            if let Some(album) = non_empty(row.media_album.as_deref()) {
                extras.insert("Album".into(), json!(album));
            }
            extras.insert("media_type".into(), json!(row.media_type));
            Shape {
                kind: "knowledge_media_usage",
                title,
                description: Some(format!("{app} ({bundle_id})")),
                extras: Value::Object(extras),
            }
        }
        STREAM_NOTIFICATION => {
            let bundle_id = non_empty(row.notification_bundle_id.as_deref()).ok_or(Skip::System)?;
            if let Some(host) = bundle_id.strip_prefix("_WEB_CENTER_:") {
                let host = host.trim();
                if ctx.web_effective && !host.is_empty() && !is_suppressed_domain(host, config) {
                    Shape {
                        kind: "knowledge_notification",
                        title: format!("Web notification from {host}"),
                        description: Some(host.to_owned()),
                        extras: json!({ "center": "web", "domain": host }),
                    }
                } else {
                    // Web usage off, an empty host, or a suppressed host:
                    // the notification still ships, anonymously. A
                    // suppressed host degrades to the same shape the
                    // opted-out user gets, so opting *in* to web usage
                    // never yields fewer events than opting out.
                    Shape {
                        kind: "knowledge_notification",
                        title: "Web notification".to_owned(),
                        description: Some("web".to_owned()),
                        extras: json!({ "center": "web" }),
                    }
                }
            } else if bundle_id.starts_with('_') {
                // `_SYSTEM_CENTER_:<daemon>` today (accountsd,
                // DiskArbitration…): daemons, not the user. Any other
                // pseudo-bundle Apple adds is not a real bundle id either,
                // and it must reach neither `app_name` — whose cache keys
                // must stay real bundle ids — nor the wire, where an
                // unknown `_…_CENTER_:` suffix could be a host.
                return Err(Skip::System);
            } else {
                if is_suppressed_bundle(bundle_id, config) {
                    return Err(Skip::Sensitive);
                }
                let app = app_name(bundle_id);
                Shape {
                    kind: "knowledge_notification",
                    title: format!("Notification from {app}"),
                    description: Some(format!("{app} ({bundle_id})")),
                    extras: json!({
                        "center": "app",
                        "bundle_id": bundle_id,
                        "app_name": app,
                    }),
                }
            }
        }
        STREAM_BLUETOOTH => {
            floor_check(row, config, duration_secs)?;
            let name = non_empty(row.bt_name.as_deref()).unwrap_or("Bluetooth device");
            let connected = row.value_integer == Some(1);
            let state = if connected {
                "connected"
            } else {
                "disconnected"
            };
            Shape {
                kind: "knowledge_bluetooth",
                title: format!("{name} {state} for {}", humanize_duration(duration_secs)),
                description: Some(name.to_owned()),
                extras: json!({
                    "device_name": name,
                    "device_type": row.bt_device_type,
                    "is_apple_audio": row.bt_is_apple_audio.map(|v| v != 0),
                    "is_user_wearing": row.bt_is_user_wearing.map(|v| v != 0),
                    "connected": connected,
                }),
            }
        }
        STREAM_DISPLAY => {
            // Off spans are never shipped; the digest's `display_on_secs`
            // is derived from the on rows alone.
            if row.value_integer != Some(1) {
                return Err(Skip::System);
            }
            floor_check(row, config, duration_secs)?;
            Shape {
                kind: "knowledge_display",
                title: format!("Display on for {}", humanize_duration(duration_secs)),
                description: None,
                extras: json!({ "backlit": true }),
            }
        }
        STREAM_WEB_USAGE => {
            let domain = non_empty(row.web_domain.as_deref()).ok_or(Skip::System)?;
            let bundle_id = non_empty(row.value_string.as_deref()).unwrap_or("com.apple.Safari");
            if is_suppressed_domain(domain, config) || is_suppressed_bundle(bundle_id, config) {
                return Err(Skip::Sensitive);
            }
            floor_check(row, config, duration_secs)?;
            let app = app_name(bundle_id);
            Shape {
                kind: "knowledge_web_usage",
                title: format!("Browsed {domain} for {}", humanize_duration(duration_secs)),
                description: Some(format!("{domain} · {app}")),
                extras: json!({
                    "bundle_id": bundle_id,
                    "app_name": app,
                    "domain": domain,
                }),
            }
        }
        STREAM_INTENTS => {
            let bundle_id = non_empty(row.source_bundle_id.as_deref());
            if bundle_id.is_some_and(|b| is_suppressed_bundle(b, config)) {
                return Err(Skip::Sensitive);
            }
            let app = bundle_id
                .map(app_name)
                .unwrap_or_else(|| "an unknown app".to_owned());
            let verb = non_empty(row.intent_verb.as_deref())
                .or(non_empty(row.intent_class.as_deref()))
                .unwrap_or("Intent");
            Shape {
                kind: "knowledge_intent",
                title: format!("{verb} via {app}"),
                description: Some(match bundle_id {
                    Some(b) => format!("{app} ({b})"),
                    None => app.clone(),
                }),
                extras: json!({
                    "intent_class": row.intent_class,
                    "intent_verb": row.intent_verb,
                    "direction": row.intent_direction,
                    "donated_by_siri": row.intent_donated_by_siri.map(|v| v != 0),
                    "domain": row.value_string,
                    "bundle_id": bundle_id,
                }),
            }
        }
        _ => return Err(Skip::System),
    };

    // The reader only emits rows whose timestamps converted, so `wire_ts`
    // cannot fail here; the fallback exists so a future change to the
    // reader's guard surfaces as a counted skip rather than a panic.
    let occurred_at = wire_ts(row.start_unix_ms).ok_or(Skip::System)?;
    let ended_at = if is_point && row.end_unix_ms <= row.start_unix_ms {
        None
    } else {
        Some(wire_ts(row.end_unix_ms).ok_or(Skip::System)?)
    };
    let observed_at = row.created_unix_ms.and_then(wire_ts);

    let mut payload = Map::new();
    payload.insert("device_id".into(), json!(ctx.device_id));
    payload.insert("stream".into(), json!(row.stream));
    payload.insert("z_pk".into(), json!(row.pk));
    payload.insert(
        "origin".into(),
        json!(if row.synced { "synced" } else { "local" }),
    );
    payload.insert("origin_device_model".into(), json!(row.origin_device_model));
    payload.insert("tz_offset_secs".into(), json!(row.tz_offset_secs));
    if !is_point {
        payload.insert("duration_secs".into(), json!(duration_secs));
    }
    if let Value::Object(extras) = shape.extras {
        payload.extend(extras);
    }

    let source_id = match non_empty(row.uuid.as_deref()) {
        Some(uuid) => format!("knowledge:{uuid}"),
        None => format!(
            "knowledge:{}:pk:{}",
            ctx.store_uuid.unwrap_or("nostore"),
            row.pk
        ),
    };

    Ok(CaptureEvent {
        source_system: "ci-applet",
        source_id,
        kind: shape.kind.to_owned(),
        occurred_at,
        ended_at,
        observed_at,
        title: Some(shape.title),
        description: shape.description,
        payload: Value::Object(payload),
        capture_tick_id: None,
    })
}

/// The stream-specific half of an event, before the common envelope.
struct Shape {
    kind: &'static str,
    title: String,
    description: Option<String>,
    extras: Value,
}

fn floor_check(
    row: &KnowledgeRow,
    config: &KnowledgeConfig,
    duration_secs: u64,
) -> Result<(), Skip> {
    match min_duration_secs(&row.stream, config) {
        Some(min) if duration_secs < min => Err(Skip::Short),
        _ => Ok(()),
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

// ── Daily digests ───────────────────────────────────────────────────────────

/// `kind` of the per-app daily event.
pub const KIND_DAILY_APP_USAGE: &str = "knowledge_daily_app_usage";
/// `kind` of the one-per-day summary event.
pub const KIND_DAILY_SUMMARY: &str = "knowledge_daily_summary";

/// How many apps the summary names in its title and lists in `top_apps`.
const TOP_APPS: usize = 3;

/// One local day, ready to be turned into digest events: the reader's
/// totals plus the zone the day is filed under and the moment the digest
/// was built.
#[derive(Debug, Clone)]
pub struct DailyDigest<'a> {
    /// The local calendar day.
    pub date: chrono::NaiveDate,
    /// Days since 2001-01-01 — [`super::reader::date_to_day_ordinal`] of
    /// `date`; carried separately so the midnight instants are derived
    /// from the same ordinal the SQL bucketed by.
    pub day: i64,
    /// Modal `ZSECONDSFROMGMT` of the day's rows: the zone whose midnight
    /// bounds the day on the wire. On a DST-transition day this is one
    /// zone for a day that had two, so the D-midnight instant may sit up
    /// to an hour from the true local midnight (and `first_activity` can
    /// precede `occurred_at` on a fall-back day); totals are per-row and
    /// the D+1 gates use the post-switch zone, so neither is affected.
    pub tz_offset_secs: i64,
    pub usage: &'a DailyUsage,
    /// Already gated by the caller (display closure); `None` = omit.
    pub display_on_secs: Option<i64>,
    /// When the digest was assembled — `observed_at` on every event.
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

/// The per-machine segment of a digest `source_id`. A digest is "what
/// this Mac did on day D" (local rows only), but CI-Server dedups on
/// `(user_id, destination, source_type, source_system, source_id)` with no
/// update path, so an id keyed on the date alone would make a second Mac
/// on the same account lose every day the first Mac also reported —
/// silently, as `duplicate`. The config identity is the natural scope;
/// when it is unset the store file's own UUID stands in, the same
/// fallback the raw lane uses for a row without a `ZUUID`. Neither value
/// is new on the wire: `device_id` rides in every digest payload already.
fn digest_scope(ctx: &FactContext<'_>) -> String {
    if ctx.device_id != Uuid::nil() {
        ctx.device_id.to_string()
    } else {
        ctx.store_uuid.unwrap_or("nostore").to_owned()
    }
}

/// The day's digest: one `knowledge_daily_app_usage` per app (rank order)
/// followed by one `knowledge_daily_summary`, or an empty vector when no
/// app survives suppression — the day then has nothing to say and the
/// caller advances past it.
///
/// Suppression runs **before** totals, ranks and shares are computed:
/// a password manager's time is not in `total_secs`, not in anyone's
/// `share_pct`, and does not push another app's rank down, so its
/// existence cannot be inferred from the arithmetic. Every session counts,
/// however short — `short_sessions` says how many were under the raw
/// lane's floor rather than silently dropping them from the total.
///
/// `occurred_at`/`ended_at` are the UTC instants of local midnight D and
/// D+1 in `tz_offset_secs` — `wire_ts` of `day*86400 − tz` Cocoa seconds
/// — so a day filed under UTC−7 starts at `07:00:00Z`. Source ids are
/// `knowledge:daily:<device_id|store_uuid>:<date>:<bundle>` /
/// `knowledge:daily:<device_id|store_uuid>:<date>:summary` (see
/// [`digest_scope`]): the same day rebuilt tomorrow yields the same ids,
/// and CI-Server's dedup turns a re-post into `duplicate`, never a second
/// copy — while two Macs on one account, each digesting its own local
/// rows, never share an id.
pub fn daily_digest_events(digest: &DailyDigest<'_>, ctx: &FactContext<'_>) -> Vec<CaptureEvent> {
    let config = ctx.config;
    let date = digest.date.format("%Y-%m-%d").to_string();
    let scope = digest_scope(ctx);

    let apps: Vec<&DailyAppUsage> = digest
        .usage
        .apps
        .iter()
        .filter(|app| !app.bundle_id.trim().is_empty())
        .filter(|app| !is_suppressed_bundle(&app.bundle_id, config))
        .filter(|app| app.total_secs > 0)
        .collect();
    if apps.is_empty() {
        return Vec::new();
    }

    let total_secs: i64 = apps.iter().map(|app| app.total_secs).sum();
    let Some(occurred_at) = local_midnight_wire(digest.day, digest.tz_offset_secs) else {
        return Vec::new();
    };
    let Some(ended_at) = local_midnight_wire(digest.day + 1, digest.tz_offset_secs) else {
        return Vec::new();
    };
    let observed_at = wire_ts_utc(digest.observed_at);
    let share = |secs: i64| -> f64 {
        if total_secs <= 0 {
            0.0
        } else {
            ((secs as f64) * 1000.0 / (total_secs as f64)).round() / 10.0
        }
    };

    let mut events: Vec<CaptureEvent> = Vec::with_capacity(apps.len() + 1);
    let mut top: Vec<Value> = Vec::with_capacity(TOP_APPS);
    let mut first_activity: Option<f64> = None;
    let mut last_activity: Option<f64> = None;

    for (index, app) in apps.iter().enumerate() {
        let rank = index + 1;
        let name = app_name(&app.bundle_id);
        let share_pct = share(app.total_secs);
        first_activity = Some(first_activity.map_or(app.first_start, |f| f.min(app.first_start)));
        last_activity = Some(last_activity.map_or(app.last_end, |l| l.max(app.last_end)));
        if rank <= TOP_APPS {
            top.push(json!({
                "bundle_id": app.bundle_id,
                "app_name": name,
                "total_secs": app.total_secs,
                "share_pct": share_pct,
            }));
        }
        events.push(CaptureEvent {
            source_system: "ci-applet",
            source_id: format!("knowledge:daily:{scope}:{date}:{}", app.bundle_id),
            kind: KIND_DAILY_APP_USAGE.to_owned(),
            occurred_at: occurred_at.clone(),
            ended_at: Some(ended_at.clone()),
            observed_at: Some(observed_at.clone()),
            title: Some(format!(
                "{date}: {name} for {} across {} session{}",
                humanize_duration(app.total_secs.max(0) as u64),
                app.session_count,
                if app.session_count == 1 { "" } else { "s" }
            )),
            description: Some(format!("{name} ({})", app.bundle_id)),
            payload: json!({
                "device_id": ctx.device_id,
                "origin": "local",
                "tz_offset_secs": digest.tz_offset_secs,
                "date": date,
                "bundle_id": app.bundle_id,
                "app_name": name,
                "total_secs": app.total_secs,
                "session_count": app.session_count,
                "short_sessions": app.short_sessions,
                "rank": rank,
                "share_pct": share_pct,
            }),
            capture_tick_id: None,
        });
    }

    // "top Chrome (3 h 02 min), Discord, Messages" — only the leader gets
    // its duration in the title; the payload has every number.
    let top_phrase = top
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let name = entry["app_name"].as_str().unwrap_or("?");
            if i == 0 {
                format!(
                    "{name} ({})",
                    humanize_duration(entry["total_secs"].as_i64().unwrap_or(0).max(0) as u64)
                )
            } else {
                name.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut payload = Map::new();
    payload.insert("device_id".into(), json!(ctx.device_id));
    payload.insert("origin".into(), json!("local"));
    payload.insert("tz_offset_secs".into(), json!(digest.tz_offset_secs));
    payload.insert("date".into(), json!(date));
    payload.insert("total_secs".into(), json!(total_secs));
    payload.insert("app_count".into(), json!(apps.len()));
    payload.insert("top_apps".into(), Value::Array(top));
    if let Some(on) = digest.display_on_secs {
        payload.insert("display_on_secs".into(), json!(on));
    }
    payload.insert(
        "first_activity".into(),
        json!(first_activity.and_then(cocoa_wire)),
    );
    payload.insert(
        "last_activity".into(),
        json!(last_activity.and_then(cocoa_wire)),
    );
    events.push(CaptureEvent {
        source_system: "ci-applet",
        source_id: format!("knowledge:daily:{scope}:{date}:summary"),
        kind: KIND_DAILY_SUMMARY.to_owned(),
        occurred_at,
        ended_at: Some(ended_at),
        observed_at: Some(observed_at),
        title: Some(format!(
            "{date}: {} of app activity across {} app{} — top {top_phrase}",
            humanize_duration(total_secs.max(0) as u64),
            apps.len(),
            if apps.len() == 1 { "" } else { "s" }
        )),
        description: None,
        payload: Value::Object(payload),
        capture_tick_id: None,
    });
    events
}

/// UTC wire instant of local midnight beginning `day` in `tz_offset_secs`.
fn local_midnight_wire(day: i64, tz_offset_secs: i64) -> Option<String> {
    cocoa_wire(local_midnight_cocoa_secs(day, tz_offset_secs))
}

/// Cocoa seconds → wire timestamp, through the reader's range guard.
fn cocoa_wire(cocoa_secs: f64) -> Option<String> {
    cocoa_secs_to_unix_ms(cocoa_secs).and_then(wire_ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::reader::{
        FIXTURE_SCHEMA, FixtureMetadata, FixtureRow, insert_row, read_events,
    };
    use rusqlite::Connection;

    /// 2026-09-15T00:00:00Z in Cocoa seconds (hand-computed).
    const T0: f64 = 811_123_200.0;
    const T0_WIRE: &str = "2026-09-15T00:00:00.000Z";

    fn fixture_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        conn
    }

    /// Insert one fixture row and read it back through the real SQL, so the
    /// mapping tests exercise the reader's column handling too.
    fn row_via_reader(fixture: &FixtureRow) -> KnowledgeRow {
        let conn = fixture_db();
        insert_row(&conn, fixture);
        let page = read_events(&conn, 0, std::slice::from_ref(&fixture.stream), true, 10).unwrap();
        assert_eq!(page.rows.len(), 1, "fixture row must survive the reader");
        page.rows.into_iter().next().unwrap()
    }

    fn ctx<'a>(config: &'a KnowledgeConfig, web_effective: bool) -> FactContext<'a> {
        FactContext {
            config,
            device_id: Uuid::nil(),
            store_uuid: Some("D0F8C2A8-4F00-4514-AA07-746DAB1E27F2"),
            web_effective,
        }
    }

    fn map(
        fixture: &FixtureRow,
        config: &KnowledgeConfig,
        web_effective: bool,
    ) -> Result<CaptureEvent, Skip> {
        let row = row_via_reader(fixture);
        to_capture_event(&row, &ctx(config, web_effective))
    }

    fn payload_keys(event: &CaptureEvent) -> Vec<String> {
        event.payload.as_object().unwrap().keys().cloned().collect()
    }

    // ── policy ──────────────────────────────────────────────────────────

    #[test]
    fn effective_streams_is_defaults_plus_extras_minus_disabled_and_ignores_unknown_names() {
        let mut config = KnowledgeConfig::default();
        assert_eq!(effective_streams(&config), DEFAULT_STREAMS);

        config.extra_streams = vec![
            "/app/webUsage".into(),
            "/app/usage".into(),    // duplicate of a default
            "/not/a/stream".into(), // unknown: dropped
            "/display/isBacklit".into(),
        ];
        config.disabled_streams = vec!["/app/mediaUsage".into(), "/display/isBacklit".into()];
        assert_eq!(
            effective_streams(&config),
            [
                "/app/usage",
                "/notification/usage",
                "/bluetooth/isConnected",
                "/app/webUsage"
            ]
        );
    }

    #[test]
    fn suppression_matches_bundle_prefixes_case_insensitively_and_the_password_heuristic() {
        let config = KnowledgeConfig::default();
        assert!(is_suppressed_bundle("com.1password.1password", &config));
        assert!(is_suppressed_bundle("COM.1PASSWORD.1PASSWORD", &config));
        assert!(is_suppressed_bundle("com.agilebits.onepassword7", &config));
        assert!(is_suppressed_bundle("com.apple.Passwords", &config));
        assert!(is_suppressed_bundle("com.apple.keychainaccess", &config));
        // A dot-less entry is still a prefix, not an exact match: suffixed
        // variants of the same app are covered.
        assert!(is_suppressed_bundle("in.sinew.enpass", &config));
        assert!(is_suppressed_bundle("in.sinew.Enpass-Desktop", &config));
        assert!(is_suppressed_bundle(
            "com.apple.keychainaccess.helper",
            &config
        ));
        // Heuristic: any id containing `password` / `keepass`.
        assert!(is_suppressed_bundle("com.example.MyPasswordVault", &config));
        assert!(is_suppressed_bundle("org.KeePass.KeePass2", &config));
        // Not suppressed: ordinary apps, and near-misses.
        assert!(!is_suppressed_bundle("com.google.Chrome", &config));
        assert!(!is_suppressed_bundle("com.lastpasta.app", &config));
        assert!(!is_suppressed_bundle("", &config));
    }

    #[test]
    fn user_bundle_exclusions_are_exact_and_case_insensitive() {
        let config = KnowledgeConfig {
            excluded_bundle_ids: vec!["com.example.Therapy".into()],
            ..Default::default()
        };
        assert!(is_suppressed_bundle("com.example.therapy", &config));
        assert!(is_suppressed_bundle(" COM.EXAMPLE.THERAPY ", &config));
        assert!(
            !is_suppressed_bundle("com.example.therapy.helper", &config),
            "exact, not prefix"
        );
    }

    #[test]
    fn domain_suppression_is_a_label_aligned_suffix_match_on_both_lists() {
        let config = KnowledgeConfig {
            excluded_domains: vec!["example.org".into(), ".Trailing.net".into()],
            ..Default::default()
        };
        assert!(is_suppressed_domain("accounts.google.com", &config));
        assert!(is_suppressed_domain("ACCOUNTS.GOOGLE.COM", &config));
        assert!(is_suppressed_domain("acme.okta.com", &config));
        assert!(is_suppressed_domain("okta.com", &config));
        assert!(is_suppressed_domain("sub.example.org", &config));
        assert!(is_suppressed_domain("www.trailing.net", &config));
        assert!(
            !is_suppressed_domain("notokta.com", &config),
            "label-aligned, not raw suffix"
        );
        assert!(!is_suppressed_domain("google.com", &config));
        assert!(!is_suppressed_domain("", &config));
    }

    #[test]
    fn duration_floors_follow_the_per_stream_table() {
        let config = KnowledgeConfig::default(); // min_session_secs = 30
        assert_eq!(min_duration_secs("/app/usage", &config), Some(30));
        assert_eq!(min_duration_secs("/app/webUsage", &config), Some(30));
        assert_eq!(min_duration_secs("/app/mediaUsage", &config), Some(1));
        assert_eq!(
            min_duration_secs("/bluetooth/isConnected", &config),
            Some(1)
        );
        assert_eq!(min_duration_secs("/display/isBacklit", &config), Some(300));
        assert_eq!(min_duration_secs("/notification/usage", &config), None);
        assert_eq!(min_duration_secs("/app/intents", &config), None);
        let zero = KnowledgeConfig {
            min_session_secs: 0,
            ..Default::default()
        };
        assert_eq!(
            min_duration_secs("/app/usage", &zero),
            Some(1),
            "0 = every non-zero row"
        );
    }

    #[test]
    fn durations_humanize_the_way_a_person_would_say_them() {
        assert_eq!(humanize_duration(0), "0 s");
        assert_eq!(humanize_duration(45), "45 s");
        assert_eq!(humanize_duration(60), "1 min");
        assert_eq!(humanize_duration(12 * 60 + 59), "12 min");
        assert_eq!(humanize_duration(3600), "1 h 00 min");
        assert_eq!(humanize_duration(3600 + 5 * 60), "1 h 05 min");
        assert_eq!(humanize_duration(3 * 3600 + 30 * 60), "3 h 30 min");
    }

    // ── per-stream mapping ───────────────────────────────────────────────

    #[test]
    fn app_usage_maps_to_a_readable_title_with_duration_and_bundle_payload() {
        let config = KnowledgeConfig::default();
        let event = map(
            &FixtureRow::new("/app/usage", T0, T0 + 12.0 * 60.0)
                .pk(41)
                .uuid("E1AE6F80-0000-0000-0000-000000000001")
                .value_string("com.example.NotInstalled")
                .metadata(FixtureMetadata {
                    usage_trusted: Some(1),
                    usage_type: Some(1),
                    ..Default::default()
                }),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(event.source_system, "ci-applet");
        assert_eq!(
            event.source_id,
            "knowledge:E1AE6F80-0000-0000-0000-000000000001"
        );
        assert_eq!(event.kind, "knowledge_app_usage");
        assert_eq!(event.title.as_deref(), Some("Used NotInstalled for 12 min"));
        assert_eq!(
            event.description.as_deref(),
            Some("NotInstalled (com.example.NotInstalled)")
        );
        assert_eq!(event.occurred_at, T0_WIRE);
        assert_eq!(event.ended_at.as_deref(), Some("2026-09-15T00:12:00.000Z"));
        assert_eq!(
            event.observed_at.as_deref(),
            Some("2026-09-15T00:12:01.500Z")
        );
        let p = &event.payload;
        assert_eq!(p["device_id"], json!(Uuid::nil()));
        assert_eq!(p["stream"], "/app/usage");
        assert_eq!(p["z_pk"], 41);
        assert_eq!(p["origin"], "local");
        assert_eq!(p["origin_device_model"], Value::Null);
        assert_eq!(p["tz_offset_secs"], -25_200);
        assert_eq!(p["duration_secs"], 720);
        assert_eq!(p["bundle_id"], "com.example.NotInstalled");
        assert_eq!(p["app_name"], "NotInstalled");
        assert_eq!(p["usage_trusted"], true);
        assert!(event.capture_tick_id.is_none());
    }

    #[test]
    fn a_row_without_a_zuuid_gets_a_store_scoped_pk_source_id() {
        let config = KnowledgeConfig::default();
        let event = map(
            &FixtureRow::new("/app/usage", T0, T0 + 60.0)
                .pk(7)
                .value_string("com.example.App"),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(
            event.source_id,
            "knowledge:D0F8C2A8-4F00-4514-AA07-746DAB1E27F2:pk:7"
        );
    }

    #[test]
    fn media_usage_carries_exact_case_search_keys_only_when_present() {
        let config = KnowledgeConfig::default();
        let bare = map(
            &FixtureRow::new("/app/mediaUsage", T0, T0 + 5.0).value_string("com.example.Player"),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(bare.kind, "knowledge_media_usage");
        assert_eq!(
            bare.title.as_deref(),
            Some("Played media in Player for 5 s")
        );
        assert_eq!(
            bare.description.as_deref(),
            Some("Player (com.example.Player)")
        );
        let keys = payload_keys(&bare);
        assert!(!keys.contains(&"Title".to_owned()));
        assert!(!keys.contains(&"Artist".to_owned()));
        assert!(!keys.contains(&"Album".to_owned()));
        assert_eq!(bare.payload["media_type"], Value::Null);

        let tagged = map(
            &FixtureRow::new("/app/mediaUsage", T0, T0 + 4.0 * 60.0)
                .value_string("com.example.Player")
                .metadata(FixtureMetadata {
                    media_title: Some("Blue in Green".into()),
                    media_artist: Some("Miles Davis".into()),
                    media_album: Some("Kind of Blue".into()),
                    media_type: Some("audio".into()),
                    ..Default::default()
                }),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(
            tagged.title.as_deref(),
            Some("Played media in Player for 4 min — Blue in Green by Miles Davis")
        );
        assert_eq!(tagged.payload["Title"], "Blue in Green");
        assert_eq!(tagged.payload["Artist"], "Miles Davis");
        assert_eq!(tagged.payload["Album"], "Kind of Blue");
        assert_eq!(tagged.payload["media_type"], "audio");
        // Never the lower-case spellings the server would not index.
        let keys = payload_keys(&tagged);
        assert!(!keys.contains(&"title".to_owned()));
        assert!(!keys.contains(&"artist".to_owned()));
    }

    fn notification(bundle: &str) -> FixtureRow {
        FixtureRow::new("/notification/usage", T0, T0)
            .value_string("Receive")
            .metadata(FixtureMetadata {
                notification_bundle_id: Some(bundle.into()),
                ..Default::default()
            })
    }

    #[test]
    fn an_app_notification_is_a_point_event_named_after_the_app() {
        let config = KnowledgeConfig::default();
        let event = map(&notification("com.example.Chat"), &config, false).unwrap();
        assert_eq!(event.kind, "knowledge_notification");
        assert_eq!(event.title.as_deref(), Some("Notification from Chat"));
        assert_eq!(
            event.description.as_deref(),
            Some("Chat (com.example.Chat)")
        );
        assert_eq!(
            event.ended_at, None,
            "start == end: no ended_at on a point event"
        );
        assert_eq!(event.payload["center"], "app");
        assert_eq!(event.payload["bundle_id"], "com.example.Chat");
        assert_eq!(event.payload["app_name"], "Chat");
        let keys = payload_keys(&event);
        assert!(!keys.contains(&"duration_secs".to_owned()));
        assert!(
            !keys
                .iter()
                .any(|k| k.contains("notification_id") || k.contains("identifier"))
        );
    }

    #[test]
    fn a_web_notification_hides_its_host_unless_web_usage_is_effective() {
        let config = KnowledgeConfig::default();
        let hidden = map(
            &notification("_WEB_CENTER_:news.example.com"),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(hidden.title.as_deref(), Some("Web notification"));
        assert_eq!(hidden.description.as_deref(), Some("web"));
        assert_eq!(hidden.payload["center"], "web");
        assert!(!payload_keys(&hidden).contains(&"domain".to_owned()));
        assert!(
            !serde_json::to_string(&hidden)
                .unwrap()
                .contains("news.example.com")
        );

        let shown = map(
            &notification("_WEB_CENTER_:news.example.com"),
            &config,
            true,
        )
        .unwrap();
        assert_eq!(
            shown.title.as_deref(),
            Some("Web notification from news.example.com")
        );
        assert_eq!(shown.description.as_deref(), Some("news.example.com"));
        assert_eq!(shown.payload["domain"], "news.example.com");
        assert!(!payload_keys(&shown).contains(&"app_name".to_owned()));

        // A sensitive host degrades to the anonymous shape when web usage
        // is on — the event is not dropped, so opting in never costs events.
        let suppressed = map(
            &notification("_WEB_CENTER_:accounts.google.com"),
            &config,
            true,
        )
        .unwrap();
        assert_eq!(suppressed.title.as_deref(), Some("Web notification"));
        assert_eq!(suppressed.description.as_deref(), Some("web"));
        assert!(!payload_keys(&suppressed).contains(&"domain".to_owned()));
        assert!(
            !serde_json::to_string(&suppressed)
                .unwrap()
                .contains("accounts.google.com")
        );
    }

    #[test]
    fn a_system_center_notification_is_dropped_as_system() {
        let config = KnowledgeConfig::default();
        assert_eq!(
            map(
                &notification("_SYSTEM_CENTER_:com.apple.accountsd"),
                &config,
                true
            )
            .err(),
            Some(Skip::System)
        );
        // …and so is a notification row with no bundle at all.
        assert_eq!(
            map(
                &FixtureRow::new("/notification/usage", T0, T0).value_string("Receive"),
                &config,
                true
            )
            .err(),
            Some(Skip::System)
        );
    }

    /// Only `_WEB_CENTER_:` is understood; any other `_`-prefixed
    /// pseudo-bundle Apple might add is not a real bundle id. It must not
    /// be resolved through `app_name` (which caches its keys for the
    /// process) and must not ship raw, because its suffix could be a
    /// host — whatever the web-usage setting.
    #[test]
    fn an_unknown_pseudo_bundle_is_dropped_before_app_name_or_the_wire() {
        let config = KnowledgeConfig::default();
        for web in [false, true] {
            assert_eq!(
                map(
                    &notification("_FOO_CENTER_:secret.example.com"),
                    &config,
                    web
                )
                .err(),
                Some(Skip::System),
                "web_effective={web}"
            );
        }
    }

    #[test]
    fn bluetooth_spans_name_the_device_and_never_carry_an_address() {
        let config = KnowledgeConfig::default();
        let fixture = FixtureRow::new("/bluetooth/isConnected", T0, T0 + 3.0 * 3600.0 + 5.0 * 60.0)
            .value_integer(1)
            .metadata(FixtureMetadata {
                bt_name: Some("AirPods Pro".into()),
                bt_device_type: Some(20),
                bt_is_apple_audio: Some(1),
                bt_is_user_wearing: Some(1),
                ..Default::default()
            });
        let event = map(&fixture, &config, false).unwrap();
        assert_eq!(event.kind, "knowledge_bluetooth");
        assert_eq!(
            event.title.as_deref(),
            Some("AirPods Pro connected for 3 h 05 min")
        );
        assert_eq!(event.description.as_deref(), Some("AirPods Pro"));
        assert_eq!(event.payload["device_name"], "AirPods Pro");
        assert_eq!(event.payload["device_type"], 20);
        assert_eq!(event.payload["is_apple_audio"], true);
        assert_eq!(event.payload["is_user_wearing"], true);
        assert_eq!(event.payload["connected"], true);
        assert_eq!(event.payload["duration_secs"], 3 * 3600 + 5 * 60);
        let text = serde_json::to_string(&event).unwrap().to_ascii_lowercase();
        assert!(!text.contains("address"));
        assert!(!text.contains("mac"));

        let disconnected = map(
            &FixtureRow::new("/bluetooth/isConnected", T0, T0 + 90.0)
                .value_integer(0)
                .metadata(FixtureMetadata {
                    bt_name: Some("TOYOTA Prius".into()),
                    ..Default::default()
                }),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(
            disconnected.title.as_deref(),
            Some("TOYOTA Prius disconnected for 1 min")
        );
        assert_eq!(disconnected.payload["connected"], false);
        assert_eq!(disconnected.payload["is_user_wearing"], Value::Null);
    }

    #[test]
    fn display_ships_only_on_spans_of_at_least_five_minutes() {
        let config = KnowledgeConfig::default();
        let on = map(
            &FixtureRow::new("/display/isBacklit", T0, T0 + 300.0).value_integer(1),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(on.kind, "knowledge_display");
        assert_eq!(on.title.as_deref(), Some("Display on for 5 min"));
        assert_eq!(on.description, None);
        assert_eq!(on.payload["backlit"], true);

        // The flap fixture from the reference host: 4 s-quantized same-state
        // re-emissions, and off rows — none of them ship.
        assert_eq!(
            map(
                &FixtureRow::new("/display/isBacklit", T0, T0 + 299.0).value_integer(1),
                &config,
                false
            )
            .err(),
            Some(Skip::Short)
        );
        assert_eq!(
            map(
                &FixtureRow::new("/display/isBacklit", T0, T0).value_integer(1),
                &config,
                false
            )
            .err(),
            Some(Skip::Short)
        );
        assert_eq!(
            map(
                &FixtureRow::new("/display/isBacklit", T0, T0 + 4.0).value_integer(1),
                &config,
                false
            )
            .err(),
            Some(Skip::Short)
        );
        assert_eq!(
            map(
                &FixtureRow::new("/display/isBacklit", T0, T0 + 3600.0).value_integer(0),
                &config,
                false
            )
            .err(),
            Some(Skip::System)
        );
    }

    #[test]
    fn web_usage_ships_the_domain_and_has_no_url_anywhere() {
        let config = KnowledgeConfig::default();
        let event = map(
            &FixtureRow::new("/app/webUsage", T0, T0 + 45.0)
                .value_string("com.apple.Safari")
                .metadata(FixtureMetadata {
                    web_domain: Some("docs.example.com".into()),
                    ..Default::default()
                }),
            &config,
            true,
        )
        .unwrap();
        assert_eq!(event.kind, "knowledge_web_usage");
        assert_eq!(
            event.title.as_deref(),
            Some("Browsed docs.example.com for 45 s")
        );
        assert!(
            event
                .description
                .as_deref()
                .unwrap()
                .starts_with("docs.example.com · ")
        );
        assert_eq!(event.payload["domain"], "docs.example.com");
        assert_eq!(event.payload["bundle_id"], "com.apple.Safari");
        let keys = payload_keys(&event);
        assert!(
            !keys.iter().any(|k| k.to_ascii_lowercase().contains("url")),
            "{keys:?}"
        );
        assert!(!serde_json::to_string(&event).unwrap().contains("http"));

        assert_eq!(
            map(
                &FixtureRow::new("/app/webUsage", T0, T0 + 45.0)
                    .value_string("com.apple.Safari")
                    .metadata(FixtureMetadata {
                        web_domain: Some("idmsa.apple.com".into()),
                        ..Default::default()
                    }),
                &config,
                true,
            )
            .err(),
            Some(Skip::Sensitive)
        );
        // No domain → nothing to say.
        assert_eq!(
            map(
                &FixtureRow::new("/app/webUsage", T0, T0 + 45.0).value_string("com.apple.Safari"),
                &config,
                true
            )
            .err(),
            Some(Skip::System)
        );
    }

    #[test]
    fn intents_name_the_verb_and_app_and_carry_no_contact_ids() {
        let config = KnowledgeConfig::default();
        let event = map(
            &FixtureRow::new("/app/intents", T0, T0)
                .value_string("Calls")
                .source_bundle_id("com.example.Phone")
                .synced_from("DEVICE-A", Some("iPhone18,2"))
                .metadata(FixtureMetadata {
                    intent_class: Some("INStartCallIntent".into()),
                    intent_verb: Some("StartCall".into()),
                    intent_direction: Some(2),
                    intent_donated_by_siri: Some(0),
                    ..Default::default()
                }),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(event.kind, "knowledge_intent");
        assert_eq!(event.title.as_deref(), Some("StartCall via Phone"));
        assert_eq!(
            event.description.as_deref(),
            Some("Phone (com.example.Phone)")
        );
        assert_eq!(event.ended_at, None);
        assert_eq!(event.payload["origin"], "synced");
        assert_eq!(event.payload["origin_device_model"], "iPhone18,2");
        assert_eq!(event.payload["intent_class"], "INStartCallIntent");
        assert_eq!(event.payload["intent_verb"], "StartCall");
        assert_eq!(event.payload["direction"], 2);
        assert_eq!(event.payload["donated_by_siri"], false);
        assert_eq!(event.payload["domain"], "Calls");
        assert_eq!(event.payload["bundle_id"], "com.example.Phone");
        let text = serde_json::to_string(&event).unwrap().to_ascii_lowercase();
        assert!(!text.contains("contact"));
        assert!(!text.contains("serialized"));
        assert!(
            !text.contains("device-a"),
            "the raw device id never leaves the reader"
        );

        // A multi-second intent (8% of them) gets an ended_at.
        let spanned = map(
            &FixtureRow::new("/app/intents", T0, T0 + 3.0)
                .source_bundle_id("com.example.Phone")
                .synced_from("DEVICE-A", None),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(
            spanned.ended_at.as_deref(),
            Some("2026-09-15T00:00:03.000Z")
        );
        assert_eq!(spanned.title.as_deref(), Some("Intent via Phone"));
        assert!(!payload_keys(&spanned).contains(&"duration_secs".to_owned()));
    }

    #[test]
    fn an_unknown_stream_is_skipped_as_system() {
        let config = KnowledgeConfig::default();
        assert_eq!(
            map(
                &FixtureRow::new("/discoverability/signals", T0, T0 + 60.0),
                &config,
                true
            )
            .err(),
            Some(Skip::System)
        );
    }

    // ── floors ──────────────────────────────────────────────────────────

    #[test]
    fn app_usage_one_second_under_the_floor_is_short_and_exactly_the_floor_ships() {
        let config = KnowledgeConfig::default(); // 30 s
        let under = FixtureRow::new("/app/usage", T0, T0 + 29.0).value_string("com.example.App");
        assert_eq!(map(&under, &config, false).err(), Some(Skip::Short));
        let exact = FixtureRow::new("/app/usage", T0, T0 + 30.0).value_string("com.example.App");
        assert!(map(&exact, &config, false).is_ok());
        // Fractional: 29.9 s floors to 29 → short.
        let fractional =
            FixtureRow::new("/app/usage", T0, T0 + 29.9).value_string("com.example.App");
        assert_eq!(map(&fractional, &config, false).err(), Some(Skip::Short));

        let zero_floor = KnowledgeConfig {
            min_session_secs: 0,
            ..Default::default()
        };
        assert!(
            map(
                &FixtureRow::new("/app/usage", T0, T0 + 1.0).value_string("com.example.App"),
                &zero_floor,
                false
            )
            .is_ok()
        );
        assert_eq!(
            map(
                &FixtureRow::new("/app/usage", T0, T0).value_string("com.example.App"),
                &zero_floor,
                false
            )
            .err(),
            Some(Skip::Short),
            "0 means every non-zero row, not every row"
        );
    }

    #[test]
    fn media_and_bluetooth_drop_only_zero_length_rows() {
        let config = KnowledgeConfig::default();
        assert_eq!(
            map(
                &FixtureRow::new("/app/mediaUsage", T0, T0).value_string("com.example.Player"),
                &config,
                false
            )
            .err(),
            Some(Skip::Short)
        );
        assert!(
            map(
                &FixtureRow::new("/app/mediaUsage", T0, T0 + 1.0)
                    .value_string("com.example.Player"),
                &config,
                false
            )
            .is_ok()
        );
        assert_eq!(
            map(
                &FixtureRow::new("/bluetooth/isConnected", T0, T0).value_integer(1),
                &config,
                false
            )
            .err(),
            Some(Skip::Short)
        );
        assert!(
            map(
                &FixtureRow::new("/bluetooth/isConnected", T0, T0 + 1.0).value_integer(1),
                &config,
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn a_suppressed_app_is_sensitive_even_when_it_would_also_be_short() {
        let config = KnowledgeConfig::default();
        assert_eq!(
            map(
                &FixtureRow::new("/app/usage", T0, T0 + 5.0)
                    .value_string("com.1password.1password"),
                &config,
                false
            )
            .err(),
            Some(Skip::Sensitive)
        );
        assert_eq!(
            map(
                &FixtureRow::new("/app/usage", T0, T0 + 600.0)
                    .value_string("com.1password.1password"),
                &config,
                false
            )
            .err(),
            Some(Skip::Sensitive)
        );
        assert_eq!(
            map(&notification("com.bitwarden.desktop"), &config, false).err(),
            Some(Skip::Sensitive)
        );
        let user = KnowledgeConfig {
            excluded_bundle_ids: vec!["com.example.app".into()],
            ..Default::default()
        };
        assert_eq!(
            map(
                &FixtureRow::new("/app/usage", T0, T0 + 600.0).value_string("com.example.App"),
                &user,
                false
            )
            .err(),
            Some(Skip::Sensitive)
        );
    }

    // ── wire format ──────────────────────────────────────────────────────

    /// CI-Server's `z.string().datetime()` accepts only a trailing `Z`. The
    /// fixture rows carry `ZSECONDSFROMGMT = -25200`; if that offset ever
    /// leaked into a wire timestamp every batch would 400.
    #[test]
    fn wire_timestamps_are_utc_z_for_every_stream() {
        let config = KnowledgeConfig::default();
        let fixtures = vec![
            FixtureRow::new("/app/usage", T0, T0 + 60.0).value_string("com.example.App"),
            FixtureRow::new("/app/mediaUsage", T0, T0 + 60.0).value_string("com.example.Player"),
            // A 1 s span so the point stream produces an `ended_at` to check.
            FixtureRow::new("/notification/usage", T0, T0 + 1.0)
                .value_string("Receive")
                .metadata(FixtureMetadata {
                    notification_bundle_id: Some("com.example.Chat".into()),
                    ..Default::default()
                }),
            FixtureRow::new("/bluetooth/isConnected", T0, T0 + 60.0).value_integer(1),
            FixtureRow::new("/display/isBacklit", T0, T0 + 600.0).value_integer(1),
            FixtureRow::new("/app/webUsage", T0, T0 + 60.0)
                .value_string("com.apple.Safari")
                .metadata(FixtureMetadata {
                    web_domain: Some("docs.example.com".into()),
                    ..Default::default()
                }),
            FixtureRow::new("/app/intents", T0, T0 + 2.0)
                .source_bundle_id("com.example.Phone")
                .synced_from("DEVICE-A", None),
        ];
        let mut seen = std::collections::HashSet::new();
        for fixture in &fixtures {
            let event = map(fixture, &config, true)
                .unwrap_or_else(|skip| panic!("{} was skipped: {skip:?}", fixture.stream));
            assert!(
                seen.insert(event.kind.clone()),
                "duplicate kind {}",
                event.kind
            );
            assert_eq!(event.occurred_at, T0_WIRE, "{}", fixture.stream);
            assert!(event.occurred_at.ends_with('Z'));
            let ended = event
                .ended_at
                .as_deref()
                .unwrap_or_else(|| panic!("{} has no ended_at", fixture.stream));
            assert!(
                ended.ends_with('Z') && ended > event.occurred_at.as_str(),
                "{}: {ended}",
                fixture.stream
            );
            let observed = event.observed_at.as_deref().unwrap();
            assert!(
                observed.ends_with('Z') && observed >= ended,
                "{}: {observed}",
                fixture.stream
            );
            assert!(!event.occurred_at.contains("-07:00"));
            assert_eq!(event.payload["tz_offset_secs"], -25_200);
        }
        assert_eq!(seen.len(), KNOWN_STREAMS.len());
    }

    #[test]
    fn a_row_with_no_creation_date_ships_without_observed_at() {
        let config = KnowledgeConfig::default();
        let event = map(
            &FixtureRow::new("/app/usage", T0, T0 + 60.0)
                .value_string("com.example.App")
                .created(None),
            &config,
            false,
        )
        .unwrap();
        assert_eq!(event.observed_at, None);
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("observed_at").is_none(), "omitted, never null");
    }

    // ── daily digests ────────────────────────────────────────────────────

    /// 2026-09-14 as days since 2001-01-01 (hand-computed; see reader tests).
    const DAY_2026_09_14: i64 = 9_387;
    const TZ_PDT: i64 = -25_200;

    fn app(bundle: &str, total_secs: i64, sessions: u64, short: u64) -> DailyAppUsage {
        DailyAppUsage {
            bundle_id: bundle.to_owned(),
            session_count: sessions,
            total_secs,
            short_sessions: short,
            first_start: local_midnight_cocoa_secs(DAY_2026_09_14, TZ_PDT) + 9.0 * 3_600.0,
            last_end: local_midnight_cocoa_secs(DAY_2026_09_14, TZ_PDT) + 17.0 * 3_600.0,
        }
    }

    fn digest<'a>(usage: &'a DailyUsage, display_on_secs: Option<i64>) -> DailyDigest<'a> {
        DailyDigest {
            date: chrono::NaiveDate::from_ymd_opt(2026, 9, 14).unwrap(),
            day: DAY_2026_09_14,
            tz_offset_secs: TZ_PDT,
            usage,
            display_on_secs,
            observed_at: chrono::DateTime::from_timestamp(1_789_500_000, 0).unwrap(),
        }
    }

    /// Digests aggregate a whole day, so the per-row envelope keys have no
    /// single value to carry: `z_pk`, `stream`, `origin_device_model` and
    /// `duration_secs` must be absent, not `null`, and the summary ships no
    /// `description` because its title already is the one-liner. This pins
    /// the wire shape `docs/plan/06-knowledge.md` §5.4/§5.5 documents.
    #[test]
    fn digest_payloads_omit_the_per_row_envelope_keys_and_the_summary_has_no_description() {
        let config = KnowledgeConfig::default();
        let usage = DailyUsage {
            apps: vec![app("com.example.Chrome", 3_600, 12, 5)],
            display_on_secs: None,
        };
        let events = daily_digest_events(&digest(&usage, None), &ctx(&config, false));
        assert_eq!(events.len(), 2);
        for event in &events {
            let payload = event
                .payload
                .as_object()
                .expect("digest payload is an object");
            for key in ["z_pk", "stream", "origin_device_model", "duration_secs"] {
                assert!(
                    !payload.contains_key(key),
                    "{} payload must not carry per-row key {key:?}",
                    event.kind
                );
            }
        }
        assert_eq!(events[0].kind, "knowledge_daily_app_usage");
        assert!(events[0].description.is_some());
        assert_eq!(events[1].kind, "knowledge_daily_summary");
        assert_eq!(events[1].description, None);
    }

    /// A day filed under UTC−7 runs from 07:00Z to 07:00Z the next day. The
    /// instants come from the row zone, never from the host's — a build
    /// machine in another zone must produce byte-identical events.
    #[test]
    fn digest_instants_are_local_midnight_in_the_rows_zone_as_utc_z() {
        let config = KnowledgeConfig::default();
        let usage = DailyUsage {
            apps: vec![app("com.example.Chrome", 3_600, 12, 5)],
            display_on_secs: Some(30_000),
        };
        let events = daily_digest_events(&digest(&usage, Some(30_000)), &ctx(&config, false));
        assert_eq!(events.len(), 2);
        for event in &events {
            assert_eq!(event.occurred_at, "2026-09-14T07:00:00.000Z");
            assert_eq!(event.ended_at.as_deref(), Some("2026-09-15T07:00:00.000Z"));
            assert_eq!(
                event.observed_at.as_deref(),
                Some("2026-09-15T19:20:00.000Z")
            );
            assert_eq!(event.source_system, "ci-applet");
            assert_eq!(event.payload["tz_offset_secs"], TZ_PDT);
            assert_eq!(event.payload["date"], "2026-09-14");
            assert_eq!(event.payload["origin"], "local");
        }
        let per_app = &events[0];
        assert_eq!(per_app.kind, "knowledge_daily_app_usage");
        assert_eq!(
            per_app.title.as_deref(),
            Some("2026-09-14: Chrome for 1 h 00 min across 12 sessions")
        );
        assert_eq!(
            per_app.description.as_deref(),
            Some("Chrome (com.example.Chrome)")
        );
        assert_eq!(per_app.payload["rank"], 1);
        assert_eq!(per_app.payload["share_pct"], 100.0);
        assert_eq!(per_app.payload["short_sessions"], 5);
        assert_eq!(per_app.payload["session_count"], 12);
        assert_eq!(per_app.payload["total_secs"], 3_600);

        let summary = &events[1];
        assert_eq!(summary.kind, "knowledge_daily_summary");
        assert_eq!(
            summary.title.as_deref(),
            Some("2026-09-14: 1 h 00 min of app activity across 1 app — top Chrome (1 h 00 min)")
        );
        assert_eq!(summary.payload["total_secs"], 3_600);
        assert_eq!(summary.payload["app_count"], 1);
        assert_eq!(summary.payload["display_on_secs"], 30_000);
        assert_eq!(
            summary.payload["first_activity"],
            "2026-09-14T16:00:00.000Z"
        );
        assert_eq!(summary.payload["last_activity"], "2026-09-15T00:00:00.000Z");
        assert_eq!(
            summary.payload["top_apps"][0]["bundle_id"],
            "com.example.Chrome"
        );

        // Display closure not reached: the key is absent, never null.
        let without = daily_digest_events(&digest(&usage, None), &ctx(&config, false));
        assert!(without[1].payload.get("display_on_secs").is_none());
    }

    /// Suppression is applied before any arithmetic: a password manager's
    /// hours are not in the total, not in anyone's share, not in the top
    /// list, and do not push another app's rank down — otherwise the
    /// missing time would be inferable from the numbers that remain.
    #[test]
    fn suppression_removes_an_app_from_totals_shares_ranks_and_the_top_list() {
        let config = KnowledgeConfig {
            excluded_bundle_ids: vec!["com.example.Therapy".into()],
            ..Default::default()
        };
        let usage = DailyUsage {
            apps: vec![
                app("com.1password.1password", 7_200, 40, 0),
                app("com.example.therapy", 5_000, 1, 0),
                app("com.example.Chrome", 3_600, 12, 5),
                app("com.example.Discord", 1_800, 30, 20),
                app("com.example.Mail", 600, 50, 49),
                app("com.example.Terminal", 0, 3, 3),
            ],
            display_on_secs: None,
        };
        let events = daily_digest_events(&digest(&usage, None), &ctx(&config, false));
        assert_eq!(events.len(), 4, "three apps + summary");
        let serialized = serde_json::to_string(&events).unwrap();
        assert!(!serialized.contains("1password"), "{serialized}");
        assert!(
            !serialized.to_ascii_lowercase().contains("therapy"),
            "{serialized}"
        );
        assert!(
            !serialized.contains("Terminal"),
            "a zero-second app is not activity: {serialized}"
        );

        assert_eq!(events[0].payload["bundle_id"], "com.example.Chrome");
        assert_eq!(
            events[0].payload["rank"], 1,
            "suppressed apps do not hold rank 1"
        );
        assert_eq!(events[0].payload["share_pct"], 60.0);
        assert_eq!(events[1].payload["bundle_id"], "com.example.Discord");
        assert_eq!(events[1].payload["rank"], 2);
        assert_eq!(events[1].payload["share_pct"], 30.0);
        assert_eq!(events[2].payload["rank"], 3);
        assert_eq!(events[2].payload["share_pct"], 10.0);

        let summary = &events[3];
        assert_eq!(
            summary.payload["total_secs"], 6_000,
            "7 200 + 5 000 hidden secs excluded"
        );
        assert_eq!(summary.payload["app_count"], 3);
        let top = summary.payload["top_apps"].as_array().unwrap();
        assert_eq!(top.len(), 3);
        assert_eq!(top[0]["bundle_id"], "com.example.Chrome");
        assert_eq!(
            summary.title.as_deref(),
            Some(
                "2026-09-14: 1 h 40 min of app activity across 3 apps — top Chrome (1 h 00 min), Discord, Mail"
            )
        );

        // Every app suppressed → nothing at all, so the day is skipped
        // rather than summarized as "0 s across 0 apps".
        let hidden = DailyUsage {
            apps: vec![app("com.1password.1password", 7_200, 40, 0)],
            display_on_secs: Some(1),
        };
        assert!(daily_digest_events(&digest(&hidden, Some(1)), &ctx(&config, false)).is_empty());
    }

    /// CI-Server dedups on `source_id` and has no update path, so a day
    /// rebuilt later (a retried batch, a replay) must produce the *same*
    /// ids — and different days, apps, or machines must never collide.
    /// The server key is per user, not per device, so a second Mac on the
    /// same account digesting the same date needs its own id or its whole
    /// day is silently counted `duplicate`.
    #[test]
    fn digest_source_ids_are_stable_across_rebuilds_and_distinct_across_days_and_devices() {
        let config = KnowledgeConfig::default();
        let usage = DailyUsage {
            apps: vec![
                app("com.example.Chrome", 3_600, 1, 0),
                app("com.example.Discord", 60, 1, 0),
            ],
            display_on_secs: None,
        };
        let first = daily_digest_events(&digest(&usage, None), &ctx(&config, false));
        let mut later = digest(&usage, None);
        later.observed_at = chrono::DateTime::from_timestamp(1_789_600_000, 0).unwrap();
        let second = daily_digest_events(&later, &ctx(&config, false));
        let ids = |events: &[CaptureEvent]| {
            events
                .iter()
                .map(|e| e.source_id.clone())
                .collect::<Vec<_>>()
        };
        // The test context has no identity, so the store UUID is the scope.
        assert_eq!(
            ids(&first),
            vec![
                "knowledge:daily:D0F8C2A8-4F00-4514-AA07-746DAB1E27F2:2026-09-14:com.example.Chrome",
                "knowledge:daily:D0F8C2A8-4F00-4514-AA07-746DAB1E27F2:2026-09-14:com.example.Discord",
                "knowledge:daily:D0F8C2A8-4F00-4514-AA07-746DAB1E27F2:2026-09-14:summary",
            ]
        );
        assert_eq!(
            ids(&first),
            ids(&second),
            "rebuilding must not mint new ids"
        );
        assert_ne!(first[0].observed_at, second[0].observed_at);

        let mut next_day = digest(&usage, None);
        next_day.date = chrono::NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        next_day.day = DAY_2026_09_14 + 1;
        let third = daily_digest_events(&next_day, &ctx(&config, false));
        assert_eq!(
            third[2].source_id,
            "knowledge:daily:D0F8C2A8-4F00-4514-AA07-746DAB1E27F2:2026-09-15:summary"
        );
        assert_eq!(third[0].occurred_at, "2026-09-15T07:00:00.000Z");

        // A signed-in Mac scopes by its identity, and a different identity
        // is a different set of ids for the same date and apps.
        let other = FactContext {
            device_id: Uuid::from_u128(1),
            ..ctx(&config, false)
        };
        let other_mac = daily_digest_events(&digest(&usage, None), &other);
        assert_ne!(
            ids(&first),
            ids(&other_mac),
            "digest ids must be scoped per device"
        );
        assert!(
            other_mac.iter().all(|e| e
                .source_id
                .starts_with("knowledge:daily:00000000-0000-0000-0000-000000000001:2026-09-14:")),
            "{:?}",
            ids(&other_mac)
        );
        assert_eq!(
            ids(&other_mac),
            ids(&daily_digest_events(&digest(&usage, None), &other)),
            "still stable across rebuilds"
        );
    }
}
