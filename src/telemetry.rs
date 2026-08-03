//! Opt-in crash reporting.
//!
//! This module is a deliberate exception to the rule that captured data only
//! reaches machines the user owns, so it is built to be defensible rather than
//! convenient:
//!
//! * **Off unless the user turns it on.** [`AppConfig::crash_reporting`] defaults
//!   to `false`. No DSN is even read until it is `true`, so a default install
//!   makes no connection to Sentry at all — the privacy claim stays literally
//!   true for anyone who does not opt in.
//! * **Panics only.** No breadcrumbs, no user identity, no performance spans, no
//!   request bodies. Breadcrumbs are where PII leaks in practice, so the SDK is
//!   configured with a zero-length breadcrumb buffer rather than trusting
//!   ourselves to keep them clean.
//! * **Scrubbed before it leaves.** [`scrub`] visits every string-bearing field
//!   of the event. A Rust panic is unusually leaky for *this* app: backtraces
//!   carry `/Users/<name>/…`, and this crate's own error strings routinely embed
//!   the user's server URL and the paths of files being ingested
//!   (`format!("reading {}", path.display())` in the extractors, portal URLs in
//!   the OIDC chain). Uncontrolled, one panic could ship a home-directory layout
//!   and a private server address to a third party.
//! * **Through this crate's own HTTP client.** Sentry's bundled transport builds
//!   its own `reqwest` client with no `.no_proxy()`, so an ambient `HTTPS_PROXY`
//!   would be able to see crash payloads — the precise defect class that
//!   invariant 1 exists to prevent. [`ProxyFreeTransport`] reuses the same
//!   proxy-free construction as every other client in this crate.
//!
//! The DSN is compile-time (`option_env!`), so a build without `SENTRY_DSN` set
//! contains no endpoint to talk to regardless of configuration.
//!
//! # How the scrubber decides
//!
//! Redaction is *default-deny for anything shaped like a location*, with a short
//! allow-list of things that are product surface rather than user data:
//!
//! * A path is redacted **to the end of its clause, spaces included**. Stopping
//!   at the first space is what makes a scrubber leak: `Application Support`,
//!   `Screen Shot 2026-…`, `OneDrive - Firstname Lastname` and `My Documents`
//!   are the *normal* shape of macOS and Windows paths, and everything after
//!   that first space is the part that names a person or a document. The cost is
//!   that a path in the middle of a sentence takes the rest of the clause with
//!   it; the terminators (`:`, `,`, `;`, `)`, `}`, `"`, a real or `\n`-escaped
//!   newline) are chosen so that anyhow's `": cause"` chains survive intact.
//! * A URL keeps its scheme and, when the path is one of this app's own routes,
//!   its route — the authority is always redacted. `POST https://[redacted]/api/
//!   v1/events failed` still says what failed and why it mattered.
//! * A host is redacted **with or without a scheme**. The failure that most
//!   wants reporting is "server unreachable", and hyper/reqwest name the host
//!   scheme-less in that error's tail, so a scheme-anchored rule would see the
//!   redacted copy and ship the raw one. That includes the two-label form
//!   (`mydomain.com:8642`) and unbracketed IPv6 (`fe80::1`), which `Ipv6Addr`
//!   and the resolver produce where `SocketAddr` would add brackets. Both
//!   shapes collide with things worth keeping — `main.rs:88` and `std::io` are
//!   spelled the same way — so each is separated by a specific signal (a port
//!   plus a suffix that is not a source extension; a compressed `::` plus a hex
//!   digit) rather than by a general rule.
//! * Emails are redacted anywhere they appear. Identity-provider error bodies
//!   used to be spliced into this crate's error strings verbatim; `oidc.rs` now
//!   keeps only the OAuth error code, so that text is cut off at the source as
//!   well — a scrubber is the second line of defence, not the first.
//! * The names of *this* machine and its owner (username, real name, hostname,
//!   device name, home directory) are captured once at [`init`] and removed from
//!   every string. Pattern rules cannot see `user liam not authorised`; this
//!   does. It is deliberately word-boundary matched so an unlucky substring
//!   cannot shred ordinary prose.
//!
//! Over-scrubbing is a real failure too — a report nobody can act on is worth
//! nothing — so `/api/v1/events`, `/dev/null`, `src/main.rs:88:17`, `and/or` and
//! `Caused by:` chains are all left alone. The tests below pin both directions.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sentry::protocol::{
    ClientSdkInfo, Context, DebugMeta, DeviceContext, Event, Map, OsContext, RuntimeContext,
    Stacktrace, Value,
};
use sentry::{ClientOptions, Transport, TransportFactory};

/// Baked in at build time by CI. Absent in local builds, which is what we want:
/// a developer's panic should not reach the shared project.
const DSN: Option<&str> = option_env!("SENTRY_DSN");

/// Sentry's own client would ignore this crate's proxy rule.
const TIMEOUT: Duration = Duration::from_secs(10);

// ── Scrubbing ───────────────────────────────────────────────────────────────

/// Replacement for anything that could identify a person or their machine.
const REDACTED: &str = "[redacted]";

/// First path segments that are this product's own HTTP surface. These are
/// built from constants in `server_common.rs`, never from user data, and the
/// route is the single most diagnostic field in an ingest failure.
const SAFE_ROUTES: &[&str] = &[
    "api",
    "v1",
    "graphql",
    "health",
    "healthz",
    "queue",
    "oauth",
    "oauth2",
    ".well-known",
];

/// Absolute roots that belong to the OS rather than the user. Which system
/// library failed is often the whole diagnosis, and none of these name a person.
/// User-owned roots (`/Users`, `/home`, `/root`, `/Volumes`, `/tmp`, `/var`,
/// `/mnt`, `/media`, `/opt`, …) are deliberately absent.
const SAFE_SYSTEM_ROOTS: &[&str] = &[
    "usr",
    "bin",
    "sbin",
    "lib",
    "lib64",
    "etc",
    "dev",
    "proc",
    "sys",
    "System",
    "Library",
];

/// Relative roots that are this repository's own layout. Keeping them readable
/// is what makes `panicked at src/main.rs:88:17` still mean something.
const SAFE_RELATIVE_ROOTS: &[&str] = &[
    "src",
    "tests",
    "test",
    "examples",
    "benches",
    "crates",
    "target",
    "assets",
    "locales",
    "docs",
    "packaging",
    "vizier",
];

/// Suffixes that only ever appear on a private-network name.
const PRIVATE_TLDS: &[&str] = &["local", "lan", "internal", "localdomain", "arpa"];

/// Extensions that make `name.ext:123` a source location rather than a
/// `host:port`. The two shapes are identical — `<word>.<word>:<digits>` — and
/// only the suffix tells them apart, so the text that makes a panic actionable
/// (`src/main.rs:88:17`) is protected by name rather than by luck.
const LINE_NUMBERED_EXTENSIONS: &[&str] = &[
    "rs", "toml", "lock", "json", "yml", "yaml", "md", "txt", "log", "csv", "tsv", "ini", "cfg",
    "conf", "html", "htm", "xml", "py", "js", "ts", "tsx", "jsx", "go", "c", "h", "cpp", "hpp",
    "sh", "sql", "rb", "java", "kt", "swift",
];

/// Values too generic to redact: a machine called `admin` must not turn every
/// occurrence of the word "admin" into `[redacted]`.
const GENERIC_NAMES: &[&str] = &[
    "user",
    "users",
    "admin",
    "administrator",
    "root",
    "guest",
    "unknown",
    "none",
    "home",
    "host",
    "localhost",
    "local",
    "default",
    "system",
    "computer",
    "laptop",
    "desktop",
    "macbook",
    "imac",
    "mac",
    "pro",
    "air",
    "mini",
    "studio",
    "server",
    "workstation",
    "surface",
    "thinkpad",
    "windows",
    "linux",
    "ubuntu",
    "debian",
    "runner",
    "build",
    "capture",
    "companion",
];

/// Rewrite one string, removing anything that identifies a person, a machine or
/// a location on one.
fn scrub_str(input: &str) -> String {
    let patterned = redact_patterns(input);
    redact_identifiers(&patterned, local_identifiers())
}

// ── Pattern-based redaction ─────────────────────────────────────────────────

/// Walk the string once, replacing every URL, host, email and path.
fn redact_patterns(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    // The end of a redacted token starts a new one: `www.example.com/private`
    // must lose its path as well as its host, even though the character before
    // that path is an ordinary letter in the *input*.
    let mut after_match = false;
    while i < input.len() {
        if (after_match || left_boundary(input, i))
            && let Some((emit, next)) = match_secret(input, i)
        {
            debug_assert!(next > i, "a match must consume input");
            push_redaction(&mut out, &emit);
            i = next;
            after_match = true;
            continue;
        }
        let c = input[i..].chars().next().expect("i is a char boundary");
        out.push(c);
        i += c.len_utf8();
        after_match = false;
    }
    out
}

/// Append a replacement, collapsing `[redacted][redacted]` into one marker.
fn push_redaction(out: &mut String, emit: &str) {
    if emit == REDACTED && out.ends_with(REDACTED) {
        return;
    }
    out.push_str(emit);
}

/// Whether byte `i` can begin a token.
///
/// Only an alphanumeric predecessor disqualifies it — that is what keeps
/// `and/or`, `TCP/IP` and `24/7` intact. Everything else (newline, tab, brace,
/// backslash, comma, a non-ASCII letter, …) is a boundary, because the previous
/// version's short allow-list of predecessors was itself a leak: one unlisted
/// character in front of a path let the whole path through.
fn left_boundary(s: &str, i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let before = &s[..i];
    // Debug-formatted payloads carry escapes as two literal characters.
    if before.ends_with("\\n") || before.ends_with("\\t") || before.ends_with("\\r") {
        return true;
    }
    match before.chars().next_back() {
        Some(c) => !c.is_ascii_alphanumeric(),
        None => true,
    }
}

/// Try every rule at `i`, most specific first. Returns the replacement text and
/// the byte offset to resume from.
fn match_secret(s: &str, i: usize) -> Option<(String, usize)> {
    if let Some(m) = match_url(s, i) {
        return Some(m);
    }
    if let Some(end) = match_email(s, i) {
        return Some((REDACTED.to_string(), end));
    }
    if let Some(end) = match_bare_ipv6(s, i) {
        return Some((REDACTED.to_string(), end));
    }
    if let Some(end) = match_host(s, i) {
        return Some((REDACTED.to_string(), end));
    }
    match_path(s, i)
}

// ── Bare IPv6 ───────────────────────────────────────────────────────────────

/// An unbracketed IPv6 literal: `fe80::1`, `2001:db8::dead:beef`. Brackets are
/// [`match_host`]'s job; this is the shape `Ipv6Addr`'s own `Display`, resolver
/// errors and discovery logs produce.
///
/// Guarded hard, because a Rust path is the same alphabet: every group must be
/// hex, at least one must contain a digit — which is what stops `cafe::babe`
/// from looking like an address — and the literal must be either compressed
/// (`::`) or a full eight groups, so `main.rs:88:17` and `12:34:56` are not.
fn match_bare_ipv6(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    let b = rest.as_bytes();
    let mut end = 0;
    while end < b.len() && (b[end].is_ascii_hexdigit() || b[end] == b':') {
        end += 1;
    }
    // A single trailing colon is the prose's (`at fe80::1: refused`); a double
    // one is the address's own compressed tail.
    if rest[..end].ends_with(':') && !rest[..end].ends_with("::") {
        end -= 1;
    }
    let token = rest.get(..end)?;
    if token.contains(":::") || token.matches("::").count() > 1 {
        return None;
    }
    let groups: Vec<&str> = token.split(':').collect();
    let compressed = token.contains("::");
    if !(compressed && groups.len() >= 3) && groups.len() != 8 {
        return None;
    }
    if !groups
        .iter()
        .all(|g| g.is_empty() || (g.len() <= 4 && g.chars().all(|c| c.is_ascii_hexdigit())))
    {
        return None;
    }
    if !groups.iter().any(|g| g.chars().any(|c| c.is_ascii_digit())) {
        return None;
    }
    // `dead::beefy` is an identifier that happens to start like an address.
    let mut k = end;
    match rest[k..].chars().next() {
        Some(c) if c.is_alphanumeric() || c == '_' || c == '.' => return None,
        // A zone index (`fe80::1%en0`) names the interface; take it too.
        Some('%') => {
            k += 1;
            while let Some(c) = rest[k..].chars().next() {
                if !c.is_ascii_alphanumeric() {
                    break;
                }
                k += c.len_utf8();
            }
        }
        _ => {}
    }
    Some(i + k)
}

// ── URLs ────────────────────────────────────────────────────────────────────

/// `scheme://authority/path` — any scheme, not just the four we happen to use.
fn match_url(s: &str, i: usize) -> Option<(String, usize)> {
    let rest = &s[i..];
    let b = rest.as_bytes();
    if !b.first()?.is_ascii_alphabetic() {
        return None;
    }
    let mut k = 1;
    while k < b.len() && (b[k].is_ascii_alphanumeric() || matches!(b[k], b'+' | b'-' | b'.')) {
        k += 1;
    }
    if !rest[k..].starts_with("://") {
        return None;
    }
    let scheme_end = k + 3;
    let auth_end = authority_end(rest, scheme_end);
    if auth_end == scheme_end {
        // `file:///Users/…` has no authority. Keep the scheme — knowing which
        // protocol failed is diagnostic — and let the path rules handle the rest
        // on the next turn of the loop.
        return Some((rest[..scheme_end].to_string(), i + scheme_end));
    }
    let tail_end = url_tail_end(rest, auth_end);
    let mut emit = String::from(&rest[..scheme_end]);
    emit.push_str(REDACTED);
    let path = &rest[auth_end..tail_end];
    if keep_absolute(path).is_some() {
        emit.push_str(path);
    }
    Some((emit, i + tail_end))
}

/// End of the authority component, brackets (IPv6 literals) included.
fn authority_end(s: &str, from: usize) -> usize {
    let b = s.as_bytes();
    let mut k = from;
    if b.get(k) == Some(&b'[') {
        while k < b.len() && b[k] != b']' {
            k += 1;
        }
        if k < b.len() {
            k += 1;
        }
    }
    while let Some(c) = s[k..].chars().next() {
        if matches!(c, '/' | '?' | '#') || is_url_stop(c) {
            break;
        }
        k += c.len_utf8();
    }
    k
}

fn url_tail_end(s: &str, from: usize) -> usize {
    let mut k = from;
    while let Some(c) = s[k..].chars().next() {
        if is_url_stop(c) {
            break;
        }
        k += c.len_utf8();
    }
    k
}

fn is_url_stop(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | ',' | ';' | ')' | ']' | '}' | '\\')
}

// ── Emails ──────────────────────────────────────────────────────────────────

/// `local@domain.tld`. Identity-provider error bodies are spliced into this
/// crate's error strings verbatim, so this is a live path, not a hypothetical.
fn match_email(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    let b = rest.as_bytes();
    let mut k = 0;
    while k < b.len()
        && (b[k].is_ascii_alphanumeric() || matches!(b[k], b'.' | b'_' | b'%' | b'+' | b'-'))
    {
        k += 1;
    }
    if k == 0 || b.get(k) != Some(&b'@') {
        return None;
    }
    let domain_start = k + 1;
    let domain_end = trim_host_end(rest, host_end(rest, domain_start));
    let domain = rest.get(domain_start..domain_end)?;
    let labels: Vec<&str> = domain.split('.').collect();
    let tld = labels.last()?;
    let well_formed = labels.len() >= 2
        && labels.iter().all(|l| !l.is_empty())
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic());
    if !well_formed {
        return None;
    }
    Some(i + domain_end)
}

// ── Hosts ───────────────────────────────────────────────────────────────────

/// A host with or without a scheme: `core-1.tail1234.ts.net:8642`,
/// `192.168.1.47:8642`, `[2001:db8::dead:beef]:8642`, `core-1.local`.
///
/// Scheme-less is the important half. `Url::parse` failures, DNS failures and
/// connect failures all name the host without one, and those are the errors this
/// app reports most.
fn match_host(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    let b = rest.as_bytes();

    if b.first() == Some(&b'[') {
        let close = rest.find(']')?;
        let inner = &rest[1..close];
        let ipv6 = inner.contains(':')
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.' | '%'));
        if !ipv6 {
            return None;
        }
        let end = close + 1;
        return Some(i + end + port_len(rest, end));
    }

    if !b.first()?.is_ascii_alphanumeric() {
        return None;
    }
    let host_end = trim_host_end(rest, host_end(rest, 0));
    let host = &rest[..host_end];
    let port = port_len(rest, host_end);
    let labels: Vec<&str> = host.split('.').collect();
    if labels.iter().any(|l| l.is_empty()) {
        return None;
    }
    let dots = labels.len() - 1;
    let tld = labels[labels.len() - 1];
    let alpha_tld = tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic());
    let private = PRIVATE_TLDS.iter().any(|p| p.eq_ignore_ascii_case(tld));
    let named = b[0].is_ascii_alphabetic();
    // A bare `core-1:8642` is what the setup screen tells people to type, so a
    // dot-less host still counts when it carries a port and looks like a machine
    // name rather than a word (`line:88` must survive).
    let dotless_server = dots == 0
        && port > 0
        && host.len() >= 3
        && host.chars().any(|c| c.is_ascii_digit() || c == '-');
    // `mydomain.com:8642` is a server; `main.rs:88` is a panic location. A port
    // is what separates them, and the extension list keeps the panic readable.
    let two_label_server = dots == 1
        && port > 0
        && alpha_tld
        && !LINE_NUMBERED_EXTENSIONS
            .iter()
            .any(|e| e.eq_ignore_ascii_case(tld));
    let identifiable = is_ipv4(host)
        || (named && dots >= 2 && alpha_tld)
        || (named && dots >= 1 && private)
        || (named && dotless_server)
        || (named && two_label_server);
    if !identifiable {
        return None;
    }
    Some(i + host_end + port)
}

fn host_end(s: &str, from: usize) -> usize {
    let b = s.as_bytes();
    let mut k = from;
    while k < b.len() && (b[k].is_ascii_alphanumeric() || matches!(b[k], b'.' | b'-' | b'_')) {
        k += 1;
    }
    k
}

/// Drop trailing punctuation so `at core-1.local.` still parses as a host.
fn trim_host_end(s: &str, end: usize) -> usize {
    let b = s.as_bytes();
    let mut k = end;
    while k > 0 && matches!(b[k - 1], b'.' | b'-' | b'_') {
        k -= 1;
    }
    k
}

/// Length of a `:1234` port suffix at `at`, or 0.
fn port_len(s: &str, at: usize) -> usize {
    let b = s.as_bytes();
    if b.get(at) != Some(&b':') {
        return 0;
    }
    let mut k = at + 1;
    while k < b.len() && b[k].is_ascii_digit() && k - at <= 5 {
        k += 1;
    }
    if k == at + 1 { 0 } else { k - at }
}

fn is_ipv4(host: &str) -> bool {
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|o| {
            !o.is_empty()
                && o.len() <= 3
                && o.chars().all(|c| c.is_ascii_digit())
                && o.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

// ── Paths ───────────────────────────────────────────────────────────────────

fn match_path(s: &str, i: usize) -> Option<(String, usize)> {
    let rest = &s[i..];
    let b = rest.as_bytes();

    // UNC (`\\fileserver\home\…`) and scheme-relative (`//host/share/…`). Both
    // name a file server and a share layout; neither is ever product surface.
    if (rest.starts_with("//") || rest.starts_with(r"\\")) && is_segment_start(rest, 2) {
        return Some((REDACTED.to_string(), i + path_extent(rest, 2, true)));
    }

    // Windows drive: `C:\Users\…`. Guarded so the `y:\n` inside a Debug-escaped
    // "Caused by:\n" — and `regex a:\d+`, and every `scheme://` — is not a path.
    if b.len() > 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'\\'
        && is_segment_start(rest, 3)
    {
        let plain = path_end(rest, 3, true, false);
        let token = &rest[..plain];
        let real_path =
            token[2..].matches('\\').count() >= 2 || has_extension(last_segment(token));
        if !real_path {
            return None;
        }
        return Some((REDACTED.to_string(), i + path_extent(rest, 3, true)));
    }

    // Absolute POSIX path.
    if b.first() == Some(&b'/') && is_segment_start(rest, 1) {
        let plain = path_end(rest, 1, false, false);
        if let Some(keep) = keep_absolute(&rest[..plain]) {
            return Some((keep.to_string(), i + keep.len()));
        }
        return Some((REDACTED.to_string(), i + path_extent(rest, 1, false)));
    }

    // Percent-encoded absolute path: `%2FUsers%2Fliam%2F…`.
    if rest.len() > 3 && rest[..3].eq_ignore_ascii_case("%2f") && is_segment_start(rest, 3) {
        return Some((REDACTED.to_string(), i + path_extent(rest, 3, false)));
    }

    // Home-relative and explicitly relative paths. The watched-folder UI stores
    // and displays `~`-style paths, so they reach error text verbatim.
    for prefix in ["~/", r"~\", "./", r".\", "../", r"..\"] {
        if rest.starts_with(prefix) && is_segment_start(rest, prefix.len()) {
            let windows = prefix.ends_with('\\');
            return Some((
                REDACTED.to_string(),
                i + path_extent(rest, prefix.len(), windows),
            ));
        }
    }

    match_relative(s, i)
}

/// A bare relative path that names a file: `liam/notes.txt`,
/// `Users\liam\notes.docx`. Kept narrow on purpose — a separator alone is
/// `and/or` or `application/json`, so a filename is required, and this repo's
/// own layout is allow-listed so panic locations stay readable.
fn match_relative(s: &str, i: usize) -> Option<(String, usize)> {
    let rest = &s[i..];
    let first = rest.chars().next()?;
    if !(first.is_alphanumeric() || matches!(first, '_' | '.')) {
        return None;
    }
    let end = path_end(rest, 0, true, false);
    let token = &rest[..end];
    if !token.contains(['/', '\\']) {
        return None;
    }
    // `at\t/Users/…`: the escape separates prose from an absolute path rather
    // than joining two segments. Decline, and the loop reaches the `/` next —
    // where the absolute rule takes the path and leaves the prose alone.
    if ["\\n/", "\\t/", "\\r/"].iter().any(|e| token.contains(e)) {
        return None;
    }
    let segments: Vec<&str> = token.split(['/', '\\']).collect();
    if segments.iter().any(|seg| seg.is_empty()) {
        return None;
    }
    // A one-character root is more often the tail of an escape (`\t/Users/…`)
    // than a directory, and the absolute rule takes that path on the next turn.
    if segments[0].chars().count() < 2 {
        return None;
    }
    if SAFE_RELATIVE_ROOTS
        .iter()
        .any(|root| root.eq_ignore_ascii_case(segments[0]))
    {
        return None;
    }
    if !segments.iter().any(|seg| has_extension(seg)) {
        return None;
    }
    Some((REDACTED.to_string(), i + end))
}

/// Whether `s[k..]` can start a path segment.
fn is_segment_start(s: &str, k: usize) -> bool {
    match s[k..].chars().next() {
        Some(c) => !is_path_stop(c) && !matches!(c, '/' | '\\'),
        None => false,
    }
}

/// Characters that end a path in an error string.
///
/// `'` is deliberately absent: `/Volumes/Liam's Backup/…` is a real macOS
/// volume, and stopping at the apostrophe leaks the possessive name. `:` is
/// present so anyhow's `": cause"` and `main.rs:88:17` survive.
fn is_path_stop(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '<' | '>' | '|' | ';' | ',' | ')' | ']' | '}' | '`' | ':' | '\0'
        )
}

/// Whether a single space is still inside the path. Anything that could begin a
/// filename continues it; a second space or a terminator does not.
fn continues_path(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '.' | '-' | '_' | '&' | '+' | '#' | '(' | '[' | '{' | '~' | '\'' | '!' | '@' | '$'
                | '%' | '=' | '/' | '\\'
        )
}

/// A Debug-formatted string carries `\n` as two characters; treat it as the end
/// of the path so the `"…notes.txt\n\nCaused by:"` chain separator survives. Not
/// applied to Windows paths, where `\n` is `\` + a directory called `notes`.
fn escape_boundary(s: &str, k: usize) -> bool {
    let rest = &s[k..];
    rest.starts_with("\\n") || rest.starts_with("\\t") || rest.starts_with("\\r")
}

fn path_end(s: &str, from: usize, windows: bool, greedy: bool) -> usize {
    let mut k = from;
    while let Some(c) = s[k..].chars().next() {
        if !windows && escape_boundary(s, k) {
            break;
        }
        if c == ' ' {
            match s[k + 1..].chars().next() {
                Some(next) if greedy && continues_path(next) => {
                    k += 1;
                    continue;
                }
                _ => break,
            }
        }
        if is_path_stop(c) {
            break;
        }
        k += c.len_utf8();
    }
    k
}

/// How far a path reaches. Spaces are swallowed only once the token already
/// looks like a real path, so `ratio a:/b was odd` loses `/b` and keeps the
/// sentence, while `/Users/liam/Desktop/liam broza passport scan.pdf` goes in
/// full.
fn path_extent(s: &str, from: usize, windows: bool) -> usize {
    let plain = path_end(s, from, windows, false);
    let token = &s[..plain];
    let separators = token.matches(['/', '\\']).count();
    if separators >= 2 || has_extension(last_segment(token)) {
        path_end(s, from, windows, true)
    } else {
        plain
    }
}

/// The part of an absolute path that may be kept verbatim, if any.
fn keep_absolute(token: &str) -> Option<&str> {
    if !token.starts_with('/') || token.contains('?') || token.contains('#') {
        return None;
    }
    let first = token.trim_start_matches('/').split(['/', '\\']).next()?;
    let safe = SAFE_ROUTES.iter().chain(SAFE_SYSTEM_ROOTS.iter());
    safe.into_iter()
        .any(|r| r.eq_ignore_ascii_case(first))
        .then_some(token)
}

fn last_segment(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// Whether a segment looks like `name.ext`. The extension must contain a letter
/// so version fragments (`3.14.15`) are not mistaken for filenames.
fn has_extension(segment: &str) -> bool {
    match segment.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
        }
        None => false,
    }
}

// ── Known local identifiers ─────────────────────────────────────────────────

/// Captured once at [`init`] so the panic path does no syscalls, and so tests
/// exercise the pattern rules on their own merits rather than on whose machine
/// they happen to run.
static LOCAL_IDS: OnceLock<Vec<String>> = OnceLock::new();

fn local_identifiers() -> &'static [String] {
    LOCAL_IDS.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Snapshot the names of this machine and its owner.
fn install_local_identifiers() {
    let _ = LOCAL_IDS.set(identifier_set(&[
        whoami::fallible::username().ok(),
        whoami::fallible::realname().ok(),
        whoami::fallible::hostname().ok(),
        whoami::fallible::devicename().ok(),
        directories::BaseDirs::new().map(|d| d.home_dir().display().to_string()),
    ]));
}

/// Build the match list: each value, the stem of any dotted name, and the words
/// of multi-word names — minus anything too short or too generic to redact.
fn identifier_set(raw: &[Option<String>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in raw.iter().flatten() {
        push_identifier(&mut out, value, 3);
        if let Some((stem, _)) = value.split_once('.') {
            push_identifier(&mut out, stem, 3);
        }
        if value.split_whitespace().count() > 1 {
            for word in value.split_whitespace() {
                push_identifier(&mut out, word.trim_end_matches("'s"), 4);
            }
        }
    }
    // Longest first: `liams-macbook.local` must win over `liams-macbook`.
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out
}

fn push_identifier(out: &mut Vec<String>, candidate: &str, min_len: usize) {
    let candidate = candidate.trim();
    if candidate.chars().count() < min_len || !candidate.chars().any(char::is_alphanumeric) {
        return;
    }
    // "Companion Capture" and "MacBook Pro" name a product, not a person.
    let all_generic = candidate.split_whitespace().all(|word| {
        let word = word.trim_end_matches("'s");
        word.chars().count() < 3 || GENERIC_NAMES.iter().any(|g| g.eq_ignore_ascii_case(word))
    });
    if all_generic {
        return;
    }
    if !out.iter().any(|e| e.eq_ignore_ascii_case(candidate)) {
        out.push(candidate.to_string());
    }
}

/// Replace whole-word occurrences of known identifiers, case-insensitively.
fn redact_identifiers<S: AsRef<str>>(input: &str, ids: &[S]) -> String {
    if ids.is_empty() {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    'next: while i < input.len() {
        if left_boundary(input, i) {
            for id in ids {
                let id = id.as_ref();
                let end = i + id.len();
                if end <= input.len()
                    && input.is_char_boundary(end)
                    && input[i..end].eq_ignore_ascii_case(id)
                    && !input[end..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric())
                {
                    push_redaction(&mut out, REDACTED);
                    i = end;
                    continue 'next;
                }
            }
        }
        let c = input[i..].chars().next().expect("i is a char boundary");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

// ── Event scrubbing ─────────────────────────────────────────────────────────

/// Recursively scrub a JSON value in place — keys as well as values, since a
/// map keyed by "which file failed" is a natural thing to write.
fn scrub_value(v: &mut Value) {
    match v {
        Value::String(s) => *s = scrub_str(s),
        Value::Array(a) => a.iter_mut().for_each(scrub_value),
        Value::Object(o) => {
            let mut fresh = serde_json::Map::with_capacity(o.len());
            for (key, mut value) in std::mem::take(o) {
                scrub_value(&mut value);
                fresh.insert(scrub_str(&key), value);
            }
            *o = fresh;
        }
        _ => {}
    }
}

/// Every frame field that can carry a location. `abs_path` and the source
/// context are dropped rather than filtered; the rest are rewritten because
/// they are what makes a stack readable.
fn scrub_stacktrace(stacktrace: Option<&mut Stacktrace>) {
    let Some(stacktrace) = stacktrace else { return };
    for frame in stacktrace.frames.iter_mut() {
        frame.abs_path = None;
        frame.pre_context.clear();
        frame.post_context.clear();
        frame.context_line = None;
        frame.vars.clear();
        frame.filename = frame.filename.as_deref().map(scrub_str);
        frame.function = frame.function.as_deref().map(scrub_str);
        frame.symbol = frame.symbol.as_deref().map(scrub_str);
        frame.module = frame.module.as_deref().map(scrub_str);
        frame.package = frame.package.as_deref().map(scrub_str);
    }
}

/// Rebuild `contexts` from an allow-list of fields known to describe the
/// platform rather than the person. `device.name` is the machine's name and
/// `os.kernel_version` can carry a build host, so neither survives — and a
/// context type we have not vetted (including `Other`, a free-form JSON map) is
/// dropped whole.
fn safe_contexts(contexts: Map<String, Context>) -> Map<String, Context> {
    let mut safe = Map::new();
    for context in contexts.into_values() {
        let key = context.type_name().to_string();
        let kept: Context = match context {
            Context::Os(os) => OsContext {
                name: os.name.as_deref().map(scrub_str),
                version: os.version.as_deref().map(scrub_str),
                ..Default::default()
            }
            .into(),
            Context::Runtime(rt) => RuntimeContext {
                name: rt.name.as_deref().map(scrub_str),
                version: rt.version.as_deref().map(scrub_str),
                ..Default::default()
            }
            .into(),
            Context::Device(device) => DeviceContext {
                arch: device.arch.as_deref().map(scrub_str),
                ..Default::default()
            }
            .into(),
            _ => continue,
        };
        safe.insert(key, kept);
    }
    safe
}

fn scrub_map(map: Map<String, String>) -> Map<String, String> {
    map.into_iter()
        .map(|(k, v)| (scrub_str(&k), scrub_str(&v)))
        .collect()
}

/// Strip identifying data from a whole event.
///
/// Allow-list shaped wherever the field has no diagnostic value: `server_name`,
/// `user`, `request`, `modules`, `debug_meta`, `template` and the deprecated
/// top-level `stacktrace` are dropped outright rather than filtered, so a future
/// SDK version that starts populating them cannot leak by default. Everything
/// that *is* worth reading — messages, stack frames, tags, extras — is rewritten
/// by [`scrub_str`].
pub fn scrub(mut event: Event<'static>) -> Option<Event<'static>> {
    // Dropped whole. Nothing here is worth a report.
    event.server_name = None;
    event.user = None;
    event.request = None;
    // Breadcrumbs are disabled at the client, but an SDK integration can still
    // attach one; drop them rather than trust that.
    event.breadcrumbs.values.clear();
    // Module and debug-image maps carry build paths in both keys and values.
    event.modules.clear();
    event.debug_meta = Cow::Owned(DebugMeta::default());
    // A fourth `abs_path`/`context_line` container this crate never populates.
    event.template = None;
    // The deprecated top-level stacktrace; panics use `exception[].stacktrace`.
    event.stacktrace = None;
    event.sdk = event.sdk.map(|sdk| {
        Cow::Owned(ClientSdkInfo {
            name: scrub_str(&sdk.name),
            version: scrub_str(&sdk.version),
            integrations: Vec::new(),
            packages: Vec::new(),
        })
    });
    event.contexts = safe_contexts(std::mem::take(&mut event.contexts));

    // Free text.
    event.message = event.message.as_deref().map(scrub_str);
    event.culprit = event.culprit.as_deref().map(scrub_str);
    event.transaction = event.transaction.as_deref().map(scrub_str);
    event.logger = event.logger.as_deref().map(scrub_str);
    event.release = event.release.as_deref().map(|v| Cow::Owned(scrub_str(v)));
    event.dist = event.dist.as_deref().map(|v| Cow::Owned(scrub_str(v)));
    event.environment = event.environment.as_deref().map(|v| Cow::Owned(scrub_str(v)));

    // The grouping key is indexed and becomes the issue title in the UI.
    let fingerprint: Vec<Cow<'static, str>> = event
        .fingerprint
        .iter()
        .map(|f| Cow::Owned(scrub_str(f)))
        .collect();
    event.fingerprint = Cow::Owned(fingerprint);

    // `logentry` is the *other* message field, populated by the log integrations.
    if let Some(entry) = event.logentry.as_mut() {
        entry.message = scrub_str(&entry.message);
        entry.params.iter_mut().for_each(scrub_value);
    }

    for exception in event.exception.values.iter_mut() {
        exception.ty = scrub_str(&exception.ty);
        exception.value = exception.value.as_deref().map(scrub_str);
        exception.module = exception.module.as_deref().map(scrub_str);
        if let Some(mechanism) = exception.mechanism.as_mut() {
            mechanism.description = mechanism.description.as_deref().map(scrub_str);
            // Free-form; the natural place for a future integration to stash a
            // cwd or an errno context.
            mechanism.data.clear();
        }
        scrub_stacktrace(exception.stacktrace.as_mut());
        scrub_stacktrace(exception.raw_stacktrace.as_mut());
    }

    // `attach_stacktrace` fills `threads` for any event without an exception
    // stacktrace, and integrations run before `before_send` — so these frames
    // are as live as the exception ones.
    for thread in event.threads.values.iter_mut() {
        // This crate names its worker threads after what they work on.
        thread.name = None;
        scrub_stacktrace(thread.stacktrace.as_mut());
        scrub_stacktrace(thread.raw_stacktrace.as_mut());
    }

    event.tags = scrub_map(std::mem::take(&mut event.tags));
    let extra = std::mem::take(&mut event.extra);
    event.extra = extra
        .into_iter()
        .map(|(key, mut value)| {
            scrub_value(&mut value);
            (scrub_str(&key), value)
        })
        .collect();

    Some(event)
}

// ── Transport ───────────────────────────────────────────────────────────────

/// Sends events over a client built the same way as every other in this crate.
struct ProxyFreeTransport {
    client: reqwest::blocking::Client,
    dsn: sentry::types::Dsn,
}

impl Transport for ProxyFreeTransport {
    fn send_envelope(&self, envelope: sentry::Envelope) {
        let mut body = Vec::new();
        if envelope.to_writer(&mut body).is_err() {
            return;
        }
        let url = self.dsn.envelope_api_url();
        let auth = self.dsn.to_auth(Some("companion-capture")).to_string();
        // Crash reporting must never be the reason the app misbehaves: a failed
        // send is logged at debug and dropped, never retried or surfaced.
        match self
            .client
            .post(url.as_str())
            .header("X-Sentry-Auth", auth)
            .header("Content-Type", "application/x-sentry-envelope")
            .body(body)
            .send()
        {
            Ok(r) if !r.status().is_success() => {
                log::debug!("sentry envelope rejected: HTTP {}", r.status())
            }
            Err(e) => log::debug!("sentry envelope not sent: {e}"),
            _ => {}
        }
    }
}

struct ProxyFreeFactory;

impl TransportFactory for ProxyFreeFactory {
    fn create_transport(&self, options: &ClientOptions) -> Arc<dyn Transport> {
        let client = reqwest::blocking::Client::builder()
            // Invariant 1 — an inherited HTTPS_PROXY must not see crash payloads.
            .no_proxy()
            .timeout(TIMEOUT)
            .build()
            .expect("building a proxy-free blocking client cannot fail");
        Arc::new(ProxyFreeTransport {
            client,
            dsn: options.dsn.clone().expect("guard checks the DSN before init"),
        })
    }
}

// ── Init ────────────────────────────────────────────────────────────────────

/// Start crash reporting if the user asked for it and a DSN was compiled in.
///
/// Returns the guard, which must be held for the process lifetime — dropping it
/// flushes and disables the client. `None` means nothing was started, which is
/// the normal case.
#[must_use = "dropping the guard immediately disables crash reporting"]
pub fn init(enabled: bool) -> Option<sentry::ClientInitGuard> {
    if !enabled {
        return None;
    }
    let dsn = DSN?.parse::<sentry::types::Dsn>().ok()?;

    // Before the client exists, so the scrubber is never asked for names it has
    // not looked up yet.
    install_local_identifiers();

    // `ClientOptions` is #[non_exhaustive], so it cannot be built as a struct
    // literal from outside the SDK — set fields on the default instead.
    let mut options = ClientOptions::default();
    options.dsn = Some(dsn);
    options.release = sentry::release_name!();
    // Alpha builds are the point of this; separate them from any later stable
    // stream so one noisy prerelease cannot bury real reports.
    options.environment = Some(if cfg!(debug_assertions) { "dev" } else { "release" }.into());
    // The contexts integration fills `server_name` from the hostname unless it
    // is already set. `scrub` clears the field too; setting it here means the
    // hostname is never read in the first place.
    options.server_name = Some(REDACTED.into());
    // See the module docs: breadcrumbs are the usual PII leak, so there is no
    // buffer for them to accumulate in. Tracing is not compiled in at all —
    // the crate is built without the performance features — so crash-only is
    // enforced by the dependency, not just by configuration.
    options.max_breadcrumbs = 0;
    options.send_default_pii = false;
    options.attach_stacktrace = true;
    options.before_send = Some(Arc::new(scrub));
    options.transport = Some(Arc::new(ProxyFreeFactory));

    Some(sentry::init(options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentry::protocol::{Exception, Frame, LogEntry, Mechanism, TemplateInfo, Thread};

    /// Every string that must never appear in a report.
    const SECRETS: &[&str] = &[
        "liam",
        "broza",
        "Broza",
        "core-1",
        "tail1234",
        "taxes",
        "passport",
    ];

    #[track_caller]
    fn assert_clean(input: &str, out: &str, forbidden: &[&str]) {
        for needle in forbidden {
            assert!(
                !out.contains(needle),
                "leaked {needle:?}\n  in: {input}\n out: {out}"
            );
        }
    }

    #[track_caller]
    fn scrubbed(input: &str, forbidden: &[&str]) -> String {
        let out = scrub_str(input);
        assert_clean(input, &out, forbidden);
        out
    }

    #[track_caller]
    fn unchanged(input: &str) {
        assert_eq!(scrub_str(input), input, "over-scrubbed {input:?}");
    }

    fn json_of(event: Event<'static>) -> String {
        serde_json::to_string(&scrub(event).expect("scrub never drops the event")).unwrap()
    }

    /// A frame with every location-bearing field populated.
    fn dirty_frame() -> Frame {
        Frame {
            function: Some("closure in /Users/liam/Documents/taxes.pdf".into()),
            symbol: Some("_ZN/Users/liam/Documents/taxes.pdf".into()),
            module: Some("mod /Users/liam/Documents/taxes.pdf".into()),
            package: Some("/Users/liam/Documents/taxes.pdf".into()),
            filename: Some("/Users/liam/Documents/taxes.pdf".into()),
            abs_path: Some("/Users/liam/Documents/taxes.pdf".into()),
            pre_context: vec!["/Users/liam/Documents/taxes.pdf".into()],
            context_line: Some("/Users/liam/Documents/taxes.pdf".into()),
            post_context: vec!["/Users/liam/Documents/taxes.pdf".into()],
            vars: {
                let mut vars = Map::new();
                vars.insert(
                    "path".into(),
                    Value::String("/Users/liam/Documents/taxes.pdf".into()),
                );
                vars
            },
            ..Default::default()
        }
    }

    fn dirty_stacktrace() -> Stacktrace {
        Stacktrace {
            frames: vec![dirty_frame()],
            ..Default::default()
        }
    }

    // ── Paths with spaces: the headline leak ────────────────────────────────

    #[test]
    fn a_space_no_longer_ends_the_redaction() {
        let out = scrubbed(
            "reading /Users/liam/Desktop/liam broza passport scan.pdf",
            SECRETS,
        );
        assert!(!out.contains("scan.pdf"), "{out}");
        assert!(out.starts_with("reading "), "{out}");
    }

    #[test]
    fn the_onedrive_tenant_folder_is_removed() {
        let out = scrubbed(
            r"reading C:\Users\liam\OneDrive - Liam Broza\Taxes\liam-w2.pdf",
            SECRETS,
        );
        assert!(!out.contains("Taxes"), "{out}");
        assert!(!out.contains("w2"), "{out}");
    }

    #[test]
    fn the_apps_own_application_support_paths_are_removed() {
        for input in [
            "reading config at /Users/liam/Library/Application Support/Companion Capture/config.json",
            "creating config dir /Users/liam/Library/Application Support/Companion Capture",
            "opening file-event queue at /Users/liam/Library/Application Support/Companion Capture/file-events.sqlite3",
            "retention: failed to prune /Users/liam/Library/Application Support/Companion Capture/screenshots/capture_20260802T151415.png",
        ] {
            let out = scrubbed(input, SECRETS);
            assert!(!out.contains("Companion Capture"), "{out}");
            assert!(!out.contains("Support"), "{out}");
        }
    }

    #[test]
    fn an_external_volume_name_survives_its_apostrophe() {
        let out = scrubbed(
            "reading /Volumes/Liam's Backup/Photos/2025/liam and kids.heic",
            SECRETS,
        );
        assert!(!out.contains("'s Backup"), "{out}");
        assert!(!out.contains("kids"), "{out}");
    }

    #[test]
    fn screenshot_filenames_do_not_pin_the_user_to_a_second() {
        let out = scrubbed(
            "file disappeared: /Users/liam/Desktop/Screen Shot 2026-08-02 at 3.14.15 PM.png",
            SECRETS,
        );
        assert!(!out.contains("Screen Shot"), "{out}");
        assert!(!out.contains("3.14.15"), "{out}");
        assert!(out.starts_with("file disappeared: "), "{out}");
    }

    #[test]
    fn document_titles_do_not_survive_the_extractor_lane() {
        let out = scrubbed(
            "opening EPUB at /Users/liam/Books/The Art of War.epub: invalid zip header",
            SECRETS,
        );
        assert!(!out.contains("Art of War"), "{out}");
        // The cause is the diagnosis; it has to survive.
        assert!(out.ends_with(": invalid zip header"), "{out}");
    }

    #[test]
    fn a_document_folder_and_filename_go_with_the_path() {
        let out = scrubbed("reading /Users/liam/Documents/Tax Returns 2025/ssn.pdf", SECRETS);
        assert!(!out.contains("Tax Returns"), "{out}");
        assert!(!out.contains("ssn"), "{out}");
    }

    #[test]
    fn a_surname_in_a_volume_name_is_removed() {
        let out = scrubbed("backup at /Volumes/Liam Broza Backup/2026/passport.jpg", SECRETS);
        assert!(!out.contains("Backup"), "{out}");
    }

    #[test]
    fn windows_my_documents_is_removed() {
        let out = scrubbed(r"opening C:\Users\liam\My Documents\Tax Returns\ssn.pdf", SECRETS);
        assert!(!out.contains("Documents"), "{out}");
        assert!(!out.contains("ssn"), "{out}");
    }

    // ── Path starts the old allow-list could not see ────────────────────────

    #[test]
    fn an_escaped_or_real_tab_before_a_path_is_a_boundary() {
        for input in [
            "at\t/Users/liam/Documents/taxes.pdf",
            r"at\t/Users/liam/Documents/taxes.pdf",
        ] {
            scrubbed(input, SECRETS);
        }
    }

    #[test]
    fn an_escaped_or_real_newline_before_a_path_is_a_boundary() {
        for input in [
            "panic\n/Users/liam/Documents/taxes.pdf",
            r"panic\n/Users/liam/Documents/taxes.pdf",
        ] {
            scrubbed(input, SECRETS);
        }
    }

    #[test]
    fn every_punctuation_predecessor_is_a_boundary() {
        for input in [
            "{/Users/liam/notes.txt}",
            "<*/Users/liam/notes.txt>",
            "a,/Users/liam/notes.txt",
            "-/home/liam/x",
            "wrote >/Users/liam/out.png",
            "cat|/Users/liam/out.png",
            "`/Users/liam/out.png`",
            "liam@/Users/liam/notes.txt",
        ] {
            scrubbed(input, &["/Users/liam", "/home/liam", "notes.txt", "out.png"]);
        }
    }

    #[test]
    fn a_non_ascii_character_before_a_path_is_a_boundary() {
        scrubbed("café/Users/liam/x", &["/Users", "liam"]);
    }

    #[test]
    fn a_leading_dot_or_underscore_segment_is_still_a_path() {
        for input in ["at /.ssh/liam/id_rsa", "at /_private/liam/x", "at /Ünterlagen/liam/x"] {
            scrubbed(input, &["liam", "id_rsa", "Ünterlagen"]);
        }
    }

    #[test]
    fn unc_and_drive_less_windows_paths_are_removed() {
        for input in [
            r"opening \\fileserver\home\liam\notes.docx",
            "at //server/share/liam/x",
            r"Users\liam\notes.docx",
        ] {
            scrubbed(input, &["liam", "fileserver", "notes.docx"]);
        }
    }

    #[test]
    fn tilde_and_relative_paths_are_removed() {
        for input in [
            "at ~/Documents/liam-taxes.pdf",
            "reading ../liam/notes.txt",
            "reading ./liam/notes.txt",
            "reading liam/notes.txt",
        ] {
            scrubbed(input, &["liam", "notes.txt", "taxes"]);
        }
    }

    #[test]
    fn json_escaped_and_percent_encoded_paths_are_removed() {
        scrubbed(r#"{"path":"\/Users\/liam\/x"}"#, &["Users", "liam"]);
        scrubbed("opening %2FUsers%2Fliam%2Ftaxes.pdf", &["Users", "liam", "taxes"]);
    }

    // ── Hosts, with and without a scheme ────────────────────────────────────

    #[test]
    fn a_scheme_less_portal_url_is_removed() {
        let out = scrubbed("parsing portal URL core-1.tail1234.ts.net:8642", SECRETS);
        assert!(out.starts_with("parsing portal URL "), "{out}");
    }

    #[test]
    fn a_transport_error_tail_cannot_repeat_the_host() {
        scrubbed(
            "sending event ingest request to https://core-1.tail1234.ts.net:8642/api/v1/events: \
             error trying to connect: dns error: failed to lookup address information for \
             core-1.tail1234.ts.net",
            SECRETS,
        );
    }

    #[test]
    fn bare_ip_addresses_are_removed() {
        scrubbed("dial tcp [2001:db8::dead:beef]:8642: refused", &["2001:db8", "dead:beef"]);
        scrubbed("connecting to 192.168.1.47:8642 timed out", &["192.168.1.47"]);
    }

    #[test]
    fn every_scheme_less_host_shape_is_removed() {
        for input in [
            "connecting to core-1.tail1234.ts.net:8642 failed",
            "GET core-1.tail1234.ts.net:8642/api/v1/events",
            "server_url=core-1.tail1234.ts.net:8642",
            "no server found at core-1.tail1234.ts.net:8642",
            "discovered host core-1.local via mDNS",
            "at //core-1.tail1234.ts.net:8642/api/v1",
            "www.liams-server.example.com/private",
            "dns error: failed to lookup address information for core-1.tail1234.ts.net",
            "client error (Connect) for core-1.tail1234.ts.net:8642",
            "invalid peer certificate: NotValidForName for core-1.tail1234.ts.net",
        ] {
            scrubbed(input, &["core-1", "tail1234", "liams-server", "example.com"]);
        }
    }

    #[test]
    fn ipv6_urls_do_not_end_the_token_early() {
        let out = scrubbed(
            "POST http://[2001:db8::dead:beef]/secret/path failed",
            &["2001:db8", "dead:beef", "/secret/path"],
        );
        assert!(out.starts_with("POST http://"), "{out}");
        let out = scrubbed(
            "POST https://[fe80::1%25en0]:8642/api/v1/events failed",
            &["fe80", "8642"],
        );
        assert!(out.contains("/api/v1/events"), "route lost: {out}");
    }

    #[test]
    fn a_two_label_server_with_a_port_is_removed() {
        // A user whose appliance is `mydomain.com:8642` was invisible to the
        // dots >= 2 rule that tailnet names satisfy.
        for input in [
            "connecting to mydomain.com:8642 timed out",
            "no server found at lifescope.io:8642",
            "GET mydomain.com:8642/api/v1/events failed",
        ] {
            scrubbed(input, &["mydomain", "lifescope.io"]);
        }
    }

    #[test]
    fn bare_ipv6_addresses_are_removed() {
        // `Ipv6Addr`'s own Display has no brackets, and neither do resolver
        // errors — only `SocketAddr` adds them.
        for input in [
            "dial tcp 2001:db8::dead:beef refused",
            "connecting to fe80::1 failed",
            "discovered core-1 at fe80::1%en0",
            "route to 2001:0db8:0000:0000:0000:0000:0000:0001 lost",
        ] {
            scrubbed(input, &["2001:db8", "dead:beef", "fe80::1", "0db8"]);
        }
    }

    #[test]
    fn rust_paths_and_timestamps_are_not_ipv6_addresses() {
        for input in [
            "companion_capture::ingest::extract panicked",
            "std::io::Error: entity not found",
            "f64::MAX exceeded",
            "at 12:34:56 the job failed",
            // All-hex groups with no digit: a module path, not an address.
            "cafe::babe was not found",
        ] {
            unchanged(input);
        }
    }

    #[test]
    fn source_locations_with_line_numbers_survive_the_host_rule() {
        // `name.ext:123` and `host:port` are the same shape; the panic location
        // is the half that has to stay readable.
        for input in [
            "thread 'main' panicked at src/main.rs:88:17",
            "parse error at config.json:12",
            "Cargo.toml:4 is malformed",
            "see build.log:220 for details",
        ] {
            unchanged(input);
        }
    }

    #[test]
    fn a_hostname_in_message_text_is_removed() {
        for input in [
            r#"minting api key "Companion Capture (liams-macbook.local)" failed"#,
            "on host liams-macbook.local",
        ] {
            scrubbed(input, &["liams-macbook"]);
        }
    }

    // ── Identities in response bodies ───────────────────────────────────────

    #[test]
    fn an_ingest_error_body_loses_its_email() {
        let out = scrubbed(
            "event ingest to https://core-1.tail1234.ts.net:8642/api/v1/events failed with HTTP \
             401 Unauthorized: {\"error\":\"unauthorized\",\"device\":\"ci-applet-liams-macbook\",\
             \"user\":\"liam.broza@gmail.com\"}",
            &["gmail.com", "liam.broza", "core-1", "tail1234"],
        );
        assert!(out.contains("401 Unauthorized"), "{out}");
    }

    #[test]
    fn an_oidc_error_body_loses_its_email() {
        scrubbed(
            "token exchange failed: HTTP 400 Bad Request — {\"error\":\"invalid_grant\",\
             \"error_description\":\"user liam.broza@gmail.com is not a member of tenant \
             liams-org\"}",
            &["liam.broza", "gmail.com"],
        );
    }

    #[test]
    fn a_non_ascii_panic_payload_loses_its_email_and_paths() {
        scrubbed(
            // The token-endpoint body itself is cut off in `oidc.rs` now, so the
            // reported string can no longer be built; what remains is the panic
            // that body used to cause.
            "byte index 300 is not a char boundary; it is inside 'ñ' (bytes 299..301) of \
             `{\"error\":\"invalid_grant\",\"error_description\":\"liam.broza@gmail.com no \
             pertenece a la organizacion Lifescope; dispositivo ci-applet-liams-macbook; ruta \
             /Users/liam/Library/Application Support/Companion Capture; reintente\"}`",
            &["liam.broza", "gmail.com", "/Users/liam", "Application Support"],
        );
    }

    #[test]
    fn emails_are_removed_from_prose() {
        let out = scrubbed("auth failed for liam.broza@gmail.com", &["liam", "gmail"]);
        assert!(out.starts_with("auth failed for "), "{out}");
    }

    // ── Known local identifiers ─────────────────────────────────────────────

    #[test]
    fn known_local_names_are_removed_from_prose() {
        let ids = identifier_set(&[
            Some("liam".into()),
            Some("Liam Broza".into()),
            Some("liams-macbook.local".into()),
            Some("Liam's MacBook Pro".into()),
        ]);
        for input in [
            "user liam not authorised",
            "on host liams-macbook.local",
            "minting api key for ci-applet-liams-macbook",
            "signed in as Liam Broza",
        ] {
            let out = redact_identifiers(input, &ids);
            assert_clean(input, &out, &["liam", "Liam", "Broza", "macbook"]);
        }
    }

    #[test]
    fn generic_and_short_names_are_never_matched() {
        let ids = identifier_set(&[
            Some("admin".into()),
            Some("localhost".into()),
            Some("pc".into()),
            Some("Companion Capture".into()),
            Some("MacBook Pro".into()),
        ]);
        assert!(ids.is_empty(), "would shred prose: {ids:?}");
        // …and a real name is only matched on word boundaries.
        let ids = identifier_set(&[Some("ana".into())]);
        assert_eq!(redact_identifiers("analysis of ana", &ids), "analysis of [redacted]");
    }

    // ── Over-scrubbing: a report nobody can read is its own failure ─────────

    #[test]
    fn ordinary_prose_survives() {
        for input in [
            "index out of bounds: the len is 3 but the index is 7",
            "called `Option::unwrap()` on a `None` value",
            "and/or was mishandled",
        ] {
            unchanged(input);
        }
    }

    #[test]
    fn anyhow_cause_chains_survive() {
        // A real captured panic payload: unwrap on an anyhow::Error prints the
        // Debug of the chain, so the newlines arrive as two literal characters.
        let input = r#"called `Result::unwrap()` on an `Err` value: "reading /Users/liam/Documents/notes.txt\n\nCaused by:\n    Permission denied (os error 13)""#;
        let out = scrubbed(input, &["liam", "notes.txt"]);
        assert!(out.contains(r"\n\nCaused by:\n"), "chain destroyed: {out}");
        assert!(out.contains("Permission denied (os error 13)"), "{out}");
        unchanged(r"Caused by:\n    Permission denied");
    }

    #[test]
    fn api_routes_survive() {
        for input in [
            "GET /api/v1/events returned 500",
            "POST /api/v1/events returned 500",
            "GET /health timed out after 10s",
            "channel closed while draining /queue",
            "building CI-Server endpoint URL for /api/v1/events",
        ] {
            unchanged(input);
        }
    }

    #[test]
    fn system_paths_survive() {
        for input in [
            r#""/dev/null" is fine"#,
            "loading /usr/lib/libSystem.B.dylib failed",
            "/System/Library/Frameworks/CoreGraphics.framework missing",
        ] {
            unchanged(input);
        }
    }

    #[test]
    fn colons_and_drive_letters_in_prose_survive() {
        for input in [
            "ratio a:/b was odd",
            r"regex a:\d+ did not match",
            r"Note:\tsee log",
            "path:/api/v1/events",
        ] {
            let out = scrub_str(input);
            assert!(out.contains("a:") || out.contains("Note:") || out.contains("path:"), "{out}");
            assert!(!out.starts_with("Not["), "mangled: {out}");
        }
        unchanged(r"regex a:\d+ did not match");
        unchanged(r"Note:\tsee log");
    }

    #[test]
    fn the_protocol_survives_even_when_the_host_does_not() {
        for (input, keep) in [
            ("at file:///Users/liam/notes.txt", "file://"),
            ("at ftp://core-1.tail1234.ts.net/x", "ftp://"),
            ("at HTTPS://core-1.tail1234.ts.net:8642/api", "HTTPS://"),
            ("connect error: tcp://10.0.0.4:8642 refused", "tcp://"),
        ] {
            let out = scrubbed(input, &["liam", "core-1", "tail1234", "10.0.0.4"]);
            assert!(out.contains(keep), "lost the scheme: {out}");
        }
    }

    #[test]
    fn panic_locations_and_causes_survive() {
        unchanged("thread 'main' panicked at src/main.rs:88:17");
        let out = scrub_str("stat file /Users/liam/Documents/taxes.pdf: No such file or directory (os error 2)");
        assert!(out.ends_with(": No such file or directory (os error 2)"), "{out}");
        let out = scrub_str("Config {dir: /tmp/liam-cache}");
        assert!(out.ends_with("}"), "{out}");
        assert!(!out.contains("liam"), "{out}");
    }

    #[test]
    fn scrubbing_is_idempotent() {
        for input in [
            "at /Users/liam/x and https://core-1:8642/y",
            "POST https://core-1.tail1234.ts.net:8642/api/v1/events failed",
            "dial tcp [2001:db8::dead:beef]:8642: refused",
            "dial tcp 2001:db8::dead:beef refused",
            "connecting to mydomain.com:8642 timed out",
            "reading /Users/liam/Desktop/liam broza passport scan.pdf",
        ] {
            let once = scrub_str(input);
            assert_eq!(scrub_str(&once), once, "not idempotent: {input}");
        }
    }

    // ── Whole-event fields ──────────────────────────────────────────────────

    #[test]
    fn an_event_loses_hostname_user_and_source_context() {
        let mut event = Event {
            server_name: Some("liams-macbook.local".into()),
            user: Some(sentry::protocol::User {
                email: Some("liam@example.com".into()),
                ..Default::default()
            }),
            message: Some("panic at /Users/liam/src/main.rs".into()),
            ..Default::default()
        };
        event.extra.insert(
            "detail".into(),
            Value::String("wrote /Users/liam/out.png".into()),
        );

        let out = scrub(event).expect("scrub never drops the event");
        assert!(out.server_name.is_none());
        assert!(out.user.is_none(), "user must be cleared, not filtered");
        assert!(!out.message.as_deref().unwrap().contains("liam"));
        let extra = out.extra.get("detail").unwrap().as_str().unwrap();
        assert!(!extra.contains("liam"), "extra leaked: {extra}");
    }

    #[test]
    fn thread_stacktraces_are_scrubbed_like_exception_ones() {
        let mut event = Event {
            message: Some("upload failed".into()),
            ..Default::default()
        };
        event.threads.values.push(Thread {
            name: Some("worker /Users/liam/Documents/taxes.pdf".into()),
            stacktrace: Some(dirty_stacktrace()),
            raw_stacktrace: Some(dirty_stacktrace()),
            ..Default::default()
        });
        let json = json_of(event);
        assert_clean("threads", &json, SECRETS);
        assert!(!json.contains("abs_path"), "{json}");
        assert!(!json.contains("context_line"), "{json}");
    }

    #[test]
    fn exception_raw_stacktraces_are_scrubbed() {
        let mut event = Event::default();
        event.exception.values.push(Exception {
            ty: "io::Error at /Users/liam/Documents/taxes.pdf".into(),
            value: Some("reading /Users/liam/Documents/taxes.pdf".into()),
            module: Some("module /Users/liam/Documents/taxes.pdf".into()),
            stacktrace: Some(dirty_stacktrace()),
            raw_stacktrace: Some(dirty_stacktrace()),
            mechanism: Some(Mechanism {
                ty: "panic".into(),
                description: Some("desc /Users/liam/Documents/taxes.pdf".into()),
                data: {
                    let mut data = Map::new();
                    data.insert(
                        "cwd".into(),
                        Value::String("/Users/liam/Documents/taxes.pdf".into()),
                    );
                    data
                },
                ..Default::default()
            }),
            ..Default::default()
        });
        let json = json_of(event);
        assert_clean("exception", &json, SECRETS);
        assert!(!json.contains("cwd"), "mechanism data kept: {json}");
        // The mechanism type is what makes a panic group; it must survive.
        assert!(json.contains("\"panic\""), "{json}");
    }

    #[test]
    fn the_deprecated_top_level_stacktrace_is_dropped() {
        let event = Event {
            stacktrace: Some(dirty_stacktrace()),
            template: Some(TemplateInfo {
                filename: Some("/Users/liam/Documents/taxes.pdf".into()),
                abs_path: Some("/Users/liam/Documents/taxes.pdf".into()),
                context_line: Some("/Users/liam/Documents/taxes.pdf".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let out = scrub(event).expect("scrub never drops the event");
        assert!(out.stacktrace.is_none(), "stacktrace kept");
        assert!(out.template.is_none(), "template kept");
    }

    #[test]
    fn contexts_are_rebuilt_from_an_allow_list() {
        let mut event = Event::default();
        event.contexts.insert(
            "device".into(),
            DeviceContext {
                name: Some("liams-macbook.local".into()),
                model: Some("/Users/liam/Documents/taxes.pdf".into()),
                arch: Some("arm64".into()),
                ..Default::default()
            }
            .into(),
        );
        event.contexts.insert(
            "os".into(),
            OsContext {
                name: Some("macOS".into()),
                version: Some("26.0".into()),
                kernel_version: Some("Darwin liams-macbook root:/Users/liam/Documents".into()),
                ..Default::default()
            }
            .into(),
        );
        event.contexts.insert(
            "mine".into(),
            Context::Other({
                let mut other = Map::new();
                other.insert(
                    "server".into(),
                    Value::String("https://core-1.tail1234.ts.net:8642/api/v1/events".into()),
                );
                other
            }),
        );
        let json = json_of(event);
        assert_clean("contexts", &json, SECRETS);
        assert!(!json.contains("liams-macbook"), "{json}");
        assert!(!json.contains("kernel_version"), "{json}");
        assert!(!json.contains("\"mine\""), "unvetted context kept: {json}");
        // The platform is still worth knowing.
        assert!(json.contains("macOS") && json.contains("arm64"), "{json}");
    }

    #[test]
    fn logentry_message_and_params_are_scrubbed() {
        let event = Event {
            logentry: Some(LogEntry {
                message: "reading /Users/liam/Documents/taxes.pdf".into(),
                params: vec![Value::String(
                    "https://core-1.tail1234.ts.net:8642/api/v1/events".into(),
                )],
            }),
            ..Default::default()
        };
        assert_clean("logentry", &json_of(event), SECRETS);
    }

    #[test]
    fn fingerprint_culprit_and_logger_are_scrubbed() {
        let event = Event {
            fingerprint: Cow::Owned(vec![
                Cow::Owned("/Users/liam/Documents/taxes.pdf".into()),
                Cow::Owned("https://core-1.tail1234.ts.net:8642/api/v1/events".into()),
            ]),
            culprit: Some("panic at /Users/liam/Documents/taxes.pdf".into()),
            logger: Some("logger /Users/liam/Documents/taxes.pdf".into()),
            transaction: Some("tx /Users/liam/Documents/taxes.pdf".into()),
            ..Default::default()
        };
        assert_clean("fingerprint", &json_of(event), SECRETS);
    }

    #[test]
    fn release_dist_and_environment_are_scrubbed() {
        let event = Event {
            release: Some("build from /Users/liam/Documents/taxes.pdf".into()),
            dist: Some("/Users/liam/Documents/taxes.pdf".into()),
            environment: Some("env https://core-1.tail1234.ts.net:8642/api/v1/events".into()),
            ..Default::default()
        };
        assert_clean("release", &json_of(event), SECRETS);
    }

    #[test]
    fn modules_debug_meta_and_sdk_are_reduced() {
        let mut event = Event::default();
        event
            .modules
            .insert("mycrate".into(), "/Users/liam/Documents/taxes.pdf".into());
        event
            .modules
            .insert("/Users/liam/Documents/taxes.pdf".into(), "1.0".into());
        event.sdk = Some(Cow::Owned(ClientSdkInfo {
            name: "sdk /Users/liam/Documents/taxes.pdf".into(),
            version: "1".into(),
            integrations: vec!["/Users/liam/Documents/taxes.pdf".into()],
            packages: vec![],
        }));
        let json = json_of(event);
        assert_clean("modules", &json, SECRETS);
        assert!(!json.contains("modules"), "{json}");
    }

    #[test]
    fn tag_and_extra_keys_are_scrubbed_as_well_as_values() {
        let mut event = Event::default();
        event
            .tags
            .insert("/Users/liam/Documents/taxes.pdf".into(), "ok".into());
        event
            .tags
            .insert("where".into(), "/Users/liam/Documents/taxes.pdf".into());
        event.extra.insert(
            "/Users/liam/Documents/taxes.pdf".into(),
            Value::String("ok".into()),
        );
        event.extra.insert(
            "nested".into(),
            serde_json::json!({
                "a": { "b": [[{ "c": "/Users/liam/Documents/taxes.pdf" }]] },
                "url": "https://core-1.tail1234.ts.net:8642/api/v1/events",
                "/Users/liam/key": "v",
            }),
        );
        assert_clean("tags", &json_of(event), SECRETS);
    }

    #[test]
    fn a_fully_populated_event_serialises_without_an_identity() {
        let path = "/Users/liam/Documents/taxes.pdf";
        let url = "https://core-1.tail1234.ts.net:8642/api/v1/events";
        let mut event = Event {
            message: Some(format!("panic in {path}")),
            culprit: Some(format!("panic in {path}")),
            transaction: Some(path.into()),
            logger: Some(path.into()),
            server_name: Some("liams-macbook.local".into()),
            release: Some(path.into()),
            dist: Some(path.into()),
            environment: Some(url.into()),
            fingerprint: Cow::Owned(vec![Cow::Owned(path.into()), Cow::Owned(url.into())]),
            logentry: Some(LogEntry {
                message: path.into(),
                params: vec![Value::String(path.into())],
            }),
            stacktrace: Some(dirty_stacktrace()),
            template: Some(TemplateInfo {
                abs_path: Some(path.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        event.modules.insert("mod".into(), path.into());
        event.threads.values.push(Thread {
            name: Some(path.into()),
            stacktrace: Some(dirty_stacktrace()),
            raw_stacktrace: Some(dirty_stacktrace()),
            ..Default::default()
        });
        event.exception.values.push(Exception {
            ty: path.into(),
            value: Some(path.into()),
            module: Some(path.into()),
            stacktrace: Some(dirty_stacktrace()),
            raw_stacktrace: Some(dirty_stacktrace()),
            mechanism: Some(Mechanism {
                ty: "panic".into(),
                description: Some(path.into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        event.tags.insert(path.into(), path.into());
        event.extra.insert(path.into(), Value::String(path.into()));
        event.contexts.insert(
            "device".into(),
            DeviceContext {
                name: Some("liams-macbook.local".into()),
                ..Default::default()
            }
            .into(),
        );
        event.sdk = Some(Cow::Owned(ClientSdkInfo {
            name: path.into(),
            version: path.into(),
            integrations: vec![path.into()],
            packages: vec![],
        }));

        let json = json_of(event);
        assert_clean("everything", &json, SECRETS);
        assert!(!json.contains("Users"), "{json}");
    }

    #[test]
    fn a_device_id_derived_from_the_hostname_is_removed() {
        // The exact 401 body from the report, through the same two passes
        // `scrub_str` runs in production. No pattern can know that
        // `ci-applet-liams-macbook` names a person — the identifier snapshot is
        // what closes it, and this is the composition that proves it does.
        let ids = identifier_set(&[
            Some("liam".into()),
            Some("Liam Broza".into()),
            Some("liams-macbook.local".into()),
        ]);
        let input = "event ingest to https://core-1.tail1234.ts.net:8642/api/v1/events failed \
                     with HTTP 401 Unauthorized: {\"error\":\"unauthorized\",\
                     \"device\":\"ci-applet-liams-macbook\",\"user\":\"liam.broza@gmail.com\"}";
        let out = redact_identifiers(&redact_patterns(input), &ids);
        assert_clean(input, &out, &["liam", "Liam", "macbook", "core-1", "tail1234", "gmail"]);
        // …and still says what failed.
        assert!(out.contains("401 Unauthorized"), "{out}");
        assert!(out.contains("/api/v1/events"), "{out}");
    }

    /// The whole adversarial corpus, through the two passes `scrub_str` runs in
    /// production, against one broad needle list.
    ///
    /// The tests above check one input each against the needles that input was
    /// reported for — which is how the original scrubber passed while leaking:
    /// `linux_and_windows_paths_are_removed` asserted only `!contains("liam")`
    /// on a path whose username happened to precede the first space. This sweep
    /// is the guard against that failure mode; a needle that leaks anywhere
    /// fails here even if the input it leaks in was reported for something else.
    #[test]
    fn every_reported_input_survives_the_production_composition() {
        let ids = identifier_set(&[
            Some("liam".into()),
            Some("Liam Broza".into()),
            Some("liams-macbook.local".into()),
            Some("Liam's MacBook Pro".into()),
            Some("/Users/liam".into()),
        ]);
        let prod = |s: &str| redact_identifiers(&redact_patterns(s), &ids);
        let secrets = [
            "liam", "Liam", "broza", "Broza", "core-1", "tail1234", "taxes", "Taxes", "passport",
            "ssn", "kids", "Art of War", "Screen Shot", "Application Support",
            "gmail", "macbook", "2001:db8", "192.168.1.47", "fileserver", "id_rsa", "notes.txt",
            "notes.docx", "Backup", "OneDrive", "w2", "Documents", "192.168", "fe80",
            "liams-server", "example.com", "mydomain", "lifescope",
        ];
        let mut failures = Vec::new();
        for input in [
            "reading /Users/liam/Desktop/liam broza passport scan.pdf",
            r"reading C:\Users\liam\OneDrive - Liam Broza\Taxes\liam-w2.pdf",
            "reading config at /Users/liam/Library/Application Support/Companion Capture/config.json",
            "creating config dir /Users/liam/Library/Application Support/Companion Capture",
            "writing config temp file /Users/liam/Library/Application Support/Companion Capture/config.json.tmp",
            "committing config to /Users/liam/Library/Application Support/Companion Capture/config.json",
            "creating screenshot dir /Users/liam/Library/Application Support/Companion Capture/screenshots",
            "opening file-event queue at /Users/liam/Library/Application Support/Companion Capture/file-events.sqlite3",
            "retention: failed to prune /Users/liam/Library/Application Support/Companion Capture/screenshots/capture_20260802T151415.png",
            "reading /Volumes/Liam's Backup/Photos/2025/liam and kids.heic",
            "file disappeared: /Users/liam/Desktop/Screen Shot 2026-08-02 at 3.14.15 PM.png",
            "opening EPUB at /Users/liam/Books/The Art of War.epub: invalid zip header",
            "parsing portal URL core-1.tail1234.ts.net:8642",
            "sending event ingest request to https://core-1.tail1234.ts.net:8642/api/v1/events: error trying to connect: dns error: failed to lookup address information for core-1.tail1234.ts.net",
            "dial tcp [2001:db8::dead:beef]:8642: refused",
            "connecting to 192.168.1.47:8642 timed out",
            "event ingest to https://core-1.tail1234.ts.net:8642/api/v1/events failed with HTTP 401 Unauthorized: {\"error\":\"unauthorized\",\"device\":\"ci-applet-liams-macbook\",\"user\":\"liam.broza@gmail.com\"}",
            "byte index 300 is not a char boundary; it is inside 'ñ' (bytes 299..301) of `{\"error\":\"invalid_grant\",\"error_description\":\"liam.broza@gmail.com no pertenece a la organizacion Lifescope; dispositivo ci-applet-liams-macbook; ruta /Users/liam/Library/Application Support/Companion Capture; reintente\"}`",
            r#"minting api key "Companion Capture (liams-macbook.local)" failed"#,
            "device name ci-applet-liams-macbook rejected",
            "at ~/Documents/liam-taxes.pdf",
            "reading ../liam/notes.txt",
            "reading ./liam/notes.txt",
            "panic\n/Users/liam/Documents/taxes.pdf",
            r"panic\n/Users/liam/Documents/taxes.pdf",
            "at\t/Users/liam/Documents/taxes.pdf",
            r"at\t/Users/liam/Documents/taxes.pdf",
            "{/Users/liam/notes.txt}",
            "<*/Users/liam/notes.txt>",
            "a,/Users/liam/notes.txt",
            "-/home/liam/x",
            "wrote >/Users/liam/out.png",
            "cat|/Users/liam/out.png",
            "`/Users/liam/out.png`",
            "liam@/Users/liam/notes.txt",
            "café/Users/liam/x",
            "at /.ssh/liam/id_rsa",
            "at /_private/liam/x",
            "at /Ünterlagen/liam/x",
            r"opening \\fileserver\home\liam\notes.docx",
            "at //server/share/liam/x",
            r"Users\liam\notes.docx",
            "reading liam/notes.txt",
            r#"{"path":"\/Users\/liam\/x"}"#,
            "opening %2FUsers%2Fliam%2Ftaxes.pdf",
            "connecting to core-1.tail1234.ts.net:8642 failed",
            "GET core-1.tail1234.ts.net:8642/api/v1/events",
            "server_url=core-1.tail1234.ts.net:8642",
            "no server found at core-1.tail1234.ts.net:8642",
            "discovered host core-1.local via mDNS",
            "at //core-1.tail1234.ts.net:8642/api/v1",
            "www.liams-server.example.com/private",
            "dns error: failed to lookup address information for core-1.tail1234.ts.net",
            "client error (Connect) for core-1.tail1234.ts.net:8642",
            "invalid peer certificate: NotValidForName for core-1.tail1234.ts.net",
            "POST http://[2001:db8::dead:beef]/secret/path failed",
            "POST https://[fe80::1%25en0]:8642/api/v1/events failed",
            "auth failed for liam.broza@gmail.com",
            "on host liams-macbook.local",
            "user liam not authorised",
            "connecting to mydomain.com:8642 timed out",
            "dial tcp 2001:db8::dead:beef refused",
        ] {
            let out = prod(input);
            for needle in secrets {
                if out.contains(needle) {
                    failures.push(format!("LEAK {needle:?}\n  in: {input}\n out: {out}"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }

    // ── Init ────────────────────────────────────────────────────────────────

    #[test]
    fn init_is_inert_when_the_user_has_not_opted_in() {
        // The whole privacy story rests on this: no opt-in, no client, no
        // connection — regardless of whether a DSN was compiled in.
        assert!(init(false).is_none());
    }

    #[test]
    fn init_is_inert_without_a_compiled_dsn() {
        // Local and unconfigured builds have no endpoint at all.
        if DSN.is_none() {
            assert!(init(true).is_none());
        }
    }
}
