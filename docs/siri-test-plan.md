# Siri & Apple Intelligence — feasibility and test plan

> **Private & Confidential — Property of Lifescope Inc. Do not distribute.**

**Scope.** What it would take for Companion Capture to be addressable by Siri
on macOS, what is reachable today, and how each stage is tested. Written against
the WWDC26 App Intents surface.

**Related.** [`CI-Server/vsAppleIntelligence.md`](../../CI-Server/vsAppleIntelligence.md)
compares the CI platform to Apple Intelligence as an *architecture*. This
document is narrower: it is about this desktop app becoming a thing Siri can
talk to, and how we prove it works.

---

## Verdict

**Siri cannot see this app today, and closing that gap properly is a
multi-week, structural change — not a feature.**

The reason is architectural, not effort. Apple Intelligence reaches apps through
exactly one door: **App Intents**, a Swift-only framework whose types are
resolved by Xcode at build time and registered from a real app target. This repo
has:

- no `*.swift`, `*.m`, `*.xcodeproj`, or `Package.swift` — **zero** Apple-native source;
- no entitlements file anywhere;
- a `.app` assembled by `cargo-packager` from a Rust binary plus icons and an
  `Info.plist` fragment (`Cargo.toml` `[package.metadata.packager]`).

Adding App Intents means adding an Xcode-built target to a repository whose
single build pipeline also emits `.msi`, `.nsis`, `.deb` and `.AppImage`. That
fork is the cost, and it is paid before the first intent does anything.

**But the thing users actually mean by "Siri, ask my memory…" is reachable in
hours**, because Siri runs Shortcuts by name and Shortcuts can run a shell
script or open a URL. That is Stage 0/1 below, and it ships in this PR.

---

## What Siri requires now (WWDC26 state)

Apple Intelligence is not "AI inside apps" — it orchestrates apps. An app that
exposes no intents is invisible to it. Four things matter:

| Capability | What it gives you | What it costs |
|---|---|---|
| **App Intents** | The base door. Actions Siri and Shortcuts can invoke. | Swift app target. |
| **App Schemas** | Predefined structures (`messages`, `mail`, `photos`, `documents`, …) Siri already knows how to reason about, so natural language works without per-phrase training. Entities conform via `@AppEntity(schema:)`. Xcode **enforces related schemas** — adopting one often obliges you to adopt its companions. | App Intents + entity modelling. |
| **View Annotations** | On-screen awareness: map views to entities so the user can say "this one". | A real UI in an Apple UI framework. Not applicable to an egui app. |
| **Spotlight semantic index** | `IndexedEntity` contributes app content to personal-context understanding, with attribution. | Entity modelling + indexing. |

The schema domains Apple ships (documents, messages, mail, photos, contacts) are
a reasonable but imperfect fit for "everything I saw and edited". A
`memory`-shaped domain does not exist on Apple's side; CI-Server's own Intent
Catalog already defines `memory.*` / `knowledge.*` / `people.*` domains, so any
bridge is a **mapping exercise between two vocabularies**, not a fresh design.

### The one genuinely new thing that helps us

**AppIntentsTesting** (WWDC26) validates an App Intents integration through
*real system pathways* — Siri, Shortcuts and Spotlight — **without UI
automation**. Before this, testing a Siri integration meant a human talking to a
Mac. It is the reason Stage 2 below has a credible automated test column at all,
and it is the single strongest argument for doing Stage 2 properly rather than
accumulating shell hacks.

---

## Where this app stands

| | Status |
|---|---|
| Swift/Xcode target | ✗ none |
| Entitlements | ✗ none |
| App Intents | ✗ none |
| **CLI entry points** | ✓ `--search <q>`, `--open-memory`, `--login`, `--check-server` |
| **URL scheme** | ✓ `companioncapture://` registered in `packaging/macos/Info.plist` |
| Developer ID signing + notarization | ✓ working (`.github/workflows/release.yml`) — but Developer ID alone does **not** unlock SiriKit |
| Search backend | ✓ CI-Server GraphQL `search(input:)`, guarded by `JwtOrApiKeyAuthGuard`, which this app's credential already satisfies |

---

## Stage 0 — Shortcuts via CLI *(shipped in this PR)*

Siri runs Shortcuts by name. A Shortcut that runs a shell script therefore gives
you a spoken trigger with no Apple-native code at all.

```
"Hey Siri, search my memory"
   → Shortcut "Search my memory"
     → Run Shell Script:
       "/Applications/Companion Capture.app/Contents/MacOS/companion-capture" --search "$1"
```

**Test T0.1 — search opens the right URL**
```bash
"/Applications/Companion Capture.app/Contents/MacOS/companion-capture" --search "budget & forecast"
```
*Pass:* browser opens `…/memory?search=budget+%26+forecast&view=list`; the same
URL is echoed on stdout. The `&` must survive — it is the exact case that was
broken on Windows before `open_url` was fixed.
*Fail if:* the query truncates at `&`, or a 404 page opens.

**Test T0.2 — no Core configured**
Clear `server.portal_url`, rerun.
*Pass:* non-zero exit, message naming Settings. *Fail if:* a panic, or a browser
window at a nonsense URL.

**Test T0.3 — works with the tray quit**
Quit the menu-bar app entirely, rerun T0.1.
*Pass:* identical behaviour. The entry point runs in the invoking process
precisely so a Shortcut does not depend on the tray being alive.

**Test T0.4 — Siri end to end** *(manual, one-time)*
Create the Shortcut above, then say "Hey Siri, search my memory".
*Pass:* Siri runs it and the browser opens. *Fail if:* Siri offers a web search
instead — meaning the Shortcut name is not being matched; rename it to a
distinctive phrase.

---

## Stage 1 — URL scheme *(shipped in this PR)*

`companioncapture://` is registered so Shortcuts, Spotlight and other apps can
address the app without hardcoding a binary path that differs between an
installed `.app` and a local build.

**Test T1.1 — scheme is registered after install**
```bash
mdls -name kMDItemCFBundleIdentifier "/Applications/Companion Capture.app"
open "companioncapture://memory"
```
*Pass:* Launch Services resolves the scheme to this bundle.
*Fail if:* "no application set to open the URL" — usually means the app has not
been launched once since install, or the plist did not make it into the bundle.

> **Handling is not implemented yet.** The scheme is *registered* here; a
> non-Tauri Rust app needs an `NSAppleEventManager` handler for
> `kAEGetURL` to receive the event. That is Stage 1b and is deliberately not in
> this PR — registering the scheme is what unblocks testing whether Launch
> Services behaves, and it is also the clean landing spot for the abandoned
> pairing flow in CI-Server (`PairingService`, `AppletPairPage.tsx`, currently
> unrouted).

---

## Stage 2 — real App Intents *(not started; weeks)*

The only path to Siri understanding *content* rather than triggering a script.

### Shape

A thin Swift **App Intents extension** inside the `.app`, talking to the Rust
core over a local IPC socket or by invoking the same CLI. The Rust binary stays
the product; Swift is a translation layer.

```
CompanionMemoryIntents (Swift, Xcode)
  ├── MemoryEntity: AppEntity, IndexedEntity   → Spotlight semantic index
  ├── SearchMemoryIntent: AppIntent            → query CI-Server search
  ├── OpenMemoryIntent: AppIntent
  └── talks to → companion-capture (Rust)
```

### What has to be decided first

1. **Which schema domain.** `documents` is the closest Apple ships. A capture is
   not a document, so entity modelling will be lossy either way. Worth mapping
   against CI-Server's existing `memory.*` domain before writing Swift.
2. **Whether captures should enter the Spotlight semantic index at all.** This
   is the sharpest question in the document. `IndexedEntity` contributes content
   to *system-wide personal context*, i.e. screenshots of everything the user
   did become searchable by an Apple subsystem. That is in direct tension with
   the product claim that capture data reaches only the user's own machines. It
   must be an explicit, off-by-default, informed opt-in — or declined outright.
3. **How the build pipeline forks.** One `.app` would come from Xcode while
   `.deb`/`.AppImage`/`.exe` come from `cargo-packager`. Signing, notarization
   and the R2 upload script all assume one producer today.

### Test matrix (Stage 2)

| ID | Test | Method | Pass |
|---|---|---|---|
| T2.1 | Intent is discoverable | `AppIntentsTesting` | Intent appears in the system catalog |
| T2.2 | Search intent returns entities | `AppIntentsTesting` | ≥1 `MemoryEntity` for a seeded query |
| T2.3 | Entity resolution disambiguates | `AppIntentsTesting` | Two matches → disambiguation prompt, not an arbitrary pick |
| T2.4 | Shortcuts invocation | `AppIntentsTesting` | Runs without the app foregrounded |
| T2.5 | Spotlight surfaces indexed entities | `AppIntentsTesting` | Only if #2 above is answered "yes" |
| T2.6 | Unauthenticated behaviour | `AppIntentsTesting` | Actionable error, never a crash or empty success |
| T2.7 | Core unreachable | `AppIntentsTesting` | Distinguishes "offline" from "no results" — a laptop is offline often |
| T2.8 | Spoken invocation | **Manual** | Siri triggers without the exact intent title |

T2.1–T2.7 run in CI on a macOS runner. T2.8 cannot be automated; budget one
manual pass per release.

---

## Recommendation

**Ship Stage 0 and 1. Do not start Stage 2 yet.**

Stage 0 delivers what users mean by the request, costs hours, works on every
platform, and is testable in CI today. Stage 2 costs weeks, forks the build
pipeline, and its most valuable half (Spotlight indexing) raises a privacy
question that should be answered on its own terms rather than as a side effect
of wanting Siri support.

Revisit Stage 2 when either: the answer to "should captures enter the Spotlight
semantic index?" is a deliberate yes, or a macOS-only build is wanted for other
reasons and the pipeline fork is already paid for.

---

## Sources

- [WWDC26 Apple Intelligence guide](https://developer.apple.com/wwdc26/guides/apple-intelligence/)
- [Build intelligent Siri experiences with App Schemas — WWDC26](https://developer.apple.com/videos/play/wwdc2026/240/)
- [Explore advanced App Intents features for Siri and Apple Intelligence — WWDC26](https://developer.apple.com/videos/play/wwdc2026/343/)
- [App Intents framework documentation](https://developer.apple.com/documentation/appintents)
- [Apple Newsroom — new intelligence frameworks and developer tools (June 2026)](https://www.apple.com/newsroom/2026/06/apple-aids-app-development-with-new-intelligence-frameworks-and-advanced-tools/)
