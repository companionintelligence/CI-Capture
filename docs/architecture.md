<p align="center"><img src="assets/icon.svg" alt="CI-Applet" width="80" height="80"></p>

# CI-Applet — Architecture

> **Companion Intelligence — Capture layer.** This document describes how the
> applet is structured today, after the file-tracking lane landed. For the
> rollout plan and design records see [`plan/00-overview.md`](plan/00-overview.md);
> for visual identity see [`assets/brand.md`](assets/brand.md).

CI-Applet is a **cross-platform** menu-bar / system-tray applet (macOS, Windows,
Linux) that grows your **Companion Memory** by perceiving your desktop and
streaming it to your CI-Server. It runs **three capture lanes** on one shared
worker runtime, config, identity, and server connection.

## 1. System context

```mermaid
flowchart LR
    subgraph Applet["CI-Applet (tray process)"]
        direction TB
        TRAY["tao + tray-icon<br/>menu / settings"]
        WORKER["worker runtime<br/>(tokio, background thread)"]
        TRAY -- commands --> WORKER
        subgraph Lanes["capture lanes"]
            direction TB
            SHOT["📸 Screenshot lane<br/>diff-gated frames"]
            VIZ["🧭 vizier lane<br/>system/activity snapshots"]
            FILE["📄 File-tracker lane<br/>watched-folder edits"]
        end
        WORKER --> SHOT & VIZ & FILE
    end

    SHOT  -->|"POST /api/files/upload"| CI
    VIZ   -->|"POST /api/v1/events"| CI
    FILE  -->|"POST /api/v1/events"| CI

    CI[("CI-Server<br/>Companion Memory")]

    classDef lane fill:#1f6feb22,stroke:#1f6feb;
    class SHOT,VIZ,FILE lane;
```

All three lanes target the same CI-Server origin. Sign-in is **OpenID Connect
(Authorization Code + PKCE)** against the configured issuer — discovery-driven,
with a loopback redirect (see [`src/oidc.rs`](../src/oidc.rs)). The applet can
also use a CI-Server API key pasted in settings; JWT/OIDC credentials are sent
as bearer auth and CI-Server API keys as `x-api-key`.

## 2. The file-tracking lane

The third lane is the answer to *"track the important files I edit and let my AI
remember them."* It is a pipeline of small, individually-tested stages.

```mermaid
flowchart TB
    OS["OS file events<br/>FSEvents · inotify · ReadDirectoryChangesW"]:::os
    OS --> NG["notify_glue<br/>notify::Event → RawObservation<br/>(+ best-effort inode)"]
    NG --> NORM["normalizer<br/>debounce storms · collapse atomic saves<br/>· infer renames by inode"]
    NORM -->|NormalizedDecision| IG{"ignore rules?"}
    IG -->|"sensitive (.ssh, keychains…)<br/>or default (node_modules, .git…)"| DROP(["drop"]):::drop
    IG -->|allowed| SET["settle<br/>poll size+mtime until stable"]
    SET --> OUT{"settle outcome"}
    OUT -->|deleted| DEL["emit file.deleted"]
    OUT -->|"oversized / unreadable"| NOHASH["emit create/modify<br/>(no hash)"]
    OUT -->|settled| FP["fingerprint<br/>sha256 + size + mtime + inode"]
    FP --> EV["NormalizedEvent"]
    DEL --> EV
    NOHASH --> EV
    EV --> Q[("SQLite queue<br/>durable · at-least-once")]:::store
    Q --> EXT["extract → Markdown<br/>(text family; rich opt-in)"]
    EXT --> SYNC["sync engine<br/>adaptive batch + exponential backoff"]
    SYNC -->|"POST /api/v1/events<br/>kind = file_event"| CI[("CI-Server")]

    classDef os fill:#8957e522,stroke:#8957e5;
    classDef store fill:#3fb95022,stroke:#3fb950;
    classDef drop fill:#f8514922,stroke:#f85149;
```

Module map (`src/`):

| Module | Responsibility |
|--------|----------------|
| `watcher/notify_glue` | Map `notify::EventKind` → `RawObservation`, capture inode |
| `watcher/normalizer` | Debounce, collapse atomic-save sequences, infer renames |
| `watcher/settle` | Wait until a file stops being written before hashing |
| `watcher/fingerprint` | Streamed sha256 + size + mtime + inode |
| `watcher/ignore` | Two-layer rules: defaults + non-disableable sensitive list |
| `watcher/mod` | The driver that wires the above + `start_watcher` |
| `queue/rusqlite_impl` | Durable event queue (WAL SQLite) |
| `sync/batching` | `BatchController`: adaptive batch size + backoff |
| `extractors/*` | File → `ExtractedDocument` (Markdown + sections) |
| `conversion/{chunk,markdown}` | Section-aware chunking + MD assembly helpers |
| `file_tracker` | The lane runtime: owns the queue, drains, extracts, syncs |
| `server_ingest` | `ingest_file_event` → GraphQL ingest |

## 3. A single edit, end to end

An editor saving `notes.md` (atomic save: write temp, rename over target)
collapses to **one** `file.modified` event:

```mermaid
sequenceDiagram
    autonumber
    participant Ed as Editor
    participant Nf as notify
    participant Nm as Normalizer
    participant St as Settle+Fingerprint
    participant Q as SQLite queue
    participant Sy as Sync engine
    participant CI as CI-Server

    Ed->>Nf: write .notes.md.tmp, rename → notes.md
    Nf->>Nm: Create(tmp), Modify(tmp), Rename(tmp→notes.md)
    Note over Nm: collapse to one<br/>Modified(notes.md)
    Nm->>St: after 750 ms debounce
    St->>St: poll size+mtime until two reads agree
    St->>St: sha256 + size + mtime + inode
    St->>Q: enqueue NormalizedEvent (durable)
    loop every 5 s, when not backing off
        Sy->>Q: take_batch(N)
        Sy->>Sy: convert notes.md → Markdown
        Sy->>CI: POST /api/v1/events (file_event + markdown)
        CI-->>Sy: success
        Sy->>Q: ack (remove)
    end
```

If the network is down, events stay in the durable queue and drain when it
returns. A permanently-rejected ("poison") event is dropped to a dead-letter
log after `MAX_ATTEMPTS` so it can't block the queue forever.

## 4. Lane lifecycle

The file-tracking lane starts, stops, and restarts in lock-step with the worker:

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle --> Running: runnable<br/>(enabled + server credential + ≥1 folder)
    Running --> Idle: Pause / config removes prerequisites
    Running --> Running: UpdateConfig<br/>(stop → await → restart)
    Idle --> Idle: UpdateConfig (still not runnable)
    Running --> [*]: Shutdown
    Idle --> [*]: Shutdown

    note right of Running
        Restart awaits the aborted task
        before reopening the SQLite queue,
        so no two writers / double-sends.
    end note
```

## 5. Conversion: file → memory-ready Markdown

```mermaid
flowchart TB
    F["changed file"] --> R{"classify by extension"}
    R -->|"html / eml / epub<br/>(--features rich-extractors)"| RICH["rich extractor"]
    R -->|"txt · md · json · csv · tsv<br/>· source code · config"| TEXT["text extractor"]
    R -->|unsupported| META["metadata-only"]
    TEXT --> DOC["ExtractedDocument<br/>markdown + sections + diagnostics"]
    RICH --> DOC
    DOC --> CHUNK["chunk<br/>section-aware · 800-token target · 100 overlap"]
    CHUNK --> PAYLOAD["payload → CI-Server"]
    META --> PAYLOAD
```

Format coverage today:

| Family | Formats | Status |
|--------|---------|--------|
| Text / code | txt, md, json, csv, tsv, **30+ source/config languages** | ✅ default |
| Web / mail / book | html, eml, epub | ✅ opt-in (`rich-extractors`) |
| Office | docx, pptx, xlsx | ⬜ stub (planned) |
| PDF / image / audio | pdf (+OCR), images (EXIF/OCR), audio (Whisper) | ⬜ stub (planned) |

## 6. Robustness properties

These are the invariants the lane is built to hold (and what the code review
hardened):

- **No false deletes.** An oversized or transiently-unreadable changed file is
  reported as a create/modify *without* a hash — never as a deletion.
- **Durable & at-least-once.** Events persist in SQLite until the server acks;
  a crash or outage loses nothing. Duplicate `event_id`s are idempotent.
- **Bounded resources.** Concurrent settle+hash work is capped by a semaphore;
  the outbound event channel is bounded (overflow logs, never grows unbounded);
  each sync tick has a wall-clock budget so a slow server can't monopolize the
  lane.
- **No poison-event stalls.** A permanently-failing event is dead-lettered
  after `MAX_ATTEMPTS` instead of blocking the FIFO queue.
- **Clean reconfigure.** Restarting the lane awaits the old task's teardown
  before opening a new SQLite connection — no double-writers, no double-sends.

## 7. Server contract

The lane writes to CI-Server's unified capture-ingest endpoint:

```mermaid
flowchart LR
    subgraph body["POST /api/v1/events"]
        E["events[]"]
        K["kind = file_event / vizier_snapshot / vizier_patch"]
        S["source_system = ci-applet<br/>source_id = stable event id"]
        P["payload (JSON):<br/>{ event, content, device_id, session_id }"]
    end
    body --> CI[("CI-Server")]
```

`/api/v1/events` accepts arbitrary JSON payloads and uses the authenticated
server context to attribute events. Verify connectivity anytime:

```bash
cargo run -- --check-server                       # configured portal
cargo run -- --check-server --portal https://your-ci-server.example.com
```

The applet probes both `/api/...` and bare API routes, so reverse-proxied and
direct CI-Server deployments can use the same CI-Server URL field.
