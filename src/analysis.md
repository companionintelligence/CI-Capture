# analysis.rs

## Purpose
Builds screenshot analysis requests for the vision model and normalizes model output into the app's `AnalysisResult`. It enforces a strict JSON contract while tolerating minor model formatting drift.

## Components

### `VisionAnalyzer::analyze`
- **Does**: Sends the screenshot + prompt to `/chat/completions`, requests JSON output, and maps the response into `AnalysisResult`.
- **Interacts with**: `AnalysisConfig` in `config.rs` and `run_analysis_loop` in `worker.rs`.
- **Rationale**: Keeps all LLM-specific request/response behavior isolated from worker and DB code, using schema constraints plus workflow-focused prompt rules so outputs describe user intent/progress instead of random UI chrome.

### `extract_content_text`
- **Does**: Pulls assistant text from OpenAI-compatible `choices[0].message.content` shapes (string or typed content array).
- **Interacts with**: Raw completion JSON payload.

### `parse_analysis_json`
- **Does**: Parses strict JSON first, then falls back to extracting the first balanced JSON object from mixed assistant text.
- **Interacts with**: `sanitize_json_block`, `extract_first_json_object`.
- **Rationale**: Handles providers that occasionally include extra prose or wrappers despite JSON instructions.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `worker.rs` | `analyze()` returns `Result<AnalysisResult>` and errors on invalid/unrecoverable payloads | Changing return type or removing fallback defaults upstream |
| `db.rs` | `AnalysisResult` fields stay stable enough for current `ci.screen_captures` insert mapping | Renaming/removing core fields like `applications` or `contains_sensitive` |
| Model providers | Request uses OpenAI-style chat completions with `response_format: json_schema` and bounded field sizes | Removing schema constraints or changing format shape |
