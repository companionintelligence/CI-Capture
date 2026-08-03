use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::AnalysisConfig;
use crate::error_body::error_body_suffix;

// Generous headroom: reasoning-capable models (e.g. Qwen3-VL) may spend tokens
// "thinking" before emitting the JSON, so a tight budget can truncate the
// answer. The schema itself is small.
const ANALYSIS_MAX_TOKENS: u32 = 4_096;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AnalysisResult {
    pub activity_summary: Option<String>,
    pub primary_application: Option<String>,
    pub activity_category: Option<String>,
    pub contains_sensitive: bool,
    pub contains_pii: bool,
    pub applications: Vec<String>,
    pub ui_elements: Vec<String>,
    pub ocr_text: Option<String>,
    pub raw_response: serde_json::Value,
}

pub struct VisionAnalyzer {
    client: Client,
}

impl Default for VisionAnalyzer {
    fn default() -> Self {
        Self {
            client: Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(120))
                .build()
                .expect("failed to build reqwest client"),
        }
    }
}

impl VisionAnalyzer {
    pub async fn analyze(
        &self,
        config: &AnalysisConfig,
        png_bytes: &[u8],
    ) -> Result<AnalysisResult> {
        if config.endpoint.is_empty() {
            return Err(anyhow!("analysis endpoint not configured"));
        }

        let b64 = general_purpose::STANDARD.encode(png_bytes);
        let prompt = r#"You are part of a local-first AI assistant that monitors a user's desktop.
Your task is to analyze a screenshot and describe meaningful work activity.

Prioritize:
1) What the user is actively doing (task/intent),
2) What happened or changed recently (errors, edits, commands, outcomes),
3) Why it matters (blocker, progress, next likely step).

Do NOT focus on random or static UI chrome.
Only include UI elements directly relevant to the current task.
If uncertain, say so briefly instead of inventing details.

Output requirements:
- activity_summary: 2-4 concise sentences about workflow, not a UI inventory.
- primary_application: the main app driving the activity.
- activity_category: short category like "debugging", "coding", "research", "communication".
- applications: 1-4 high-signal apps involved.
- ui_elements: 2-8 task-relevant elements only.
- contains_sensitive / contains_pii: set true only when there is clear evidence.
- ocr_text: include only key visible text snippets relevant to the task.

Return ONLY JSON. Do not include markdown, comments, or reasoning.
"#;

        let payload = ChatRequest {
            model: config.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: vec![ContentBlock::Text {
                        text: "Provide structured analysis for captured desktop screenshots."
                            .into(),
                    }],
                },
                ChatMessage {
                    role: "user".into(),
                    content: vec![
                        ContentBlock::Text {
                            text: prompt.into(),
                        },
                        ContentBlock::ImageUrl {
                            image_url: ImageInput {
                                url: format!("data:image/png;base64,{}", b64),
                            },
                        },
                    ],
                },
            ],
            max_tokens: Some(ANALYSIS_MAX_TOKENS),
            temperature: Some(0.0),
            response_format: Some(ResponseFormat::analysis_json_schema()),
        };

        let url = format!("{}/chat/completions", config.endpoint.trim_end_matches('/'));
        let mut request = self.client.post(url).json(&payload);
        if !config.api_key.is_empty() {
            request = request.bearer_auth(&config.api_key);
        }

        let response = request.send().await.context("sending analysis request")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // The request we just sent held the base64 screenshot and the
            // prompt, and a local LM Studio / Ollama routinely echoes the
            // request back on an error — so the body is reduced to an error
            // code before it goes anywhere. `body` itself stays in scope for
            // the success path below.
            return Err(anyhow!(
                "vision endpoint error {status}{}",
                error_body_suffix(&body)
            ));
        }
        let raw_json: Value =
            serde_json::from_str(&body).with_context(|| "parsing analysis response".to_string())?;
        let content = extract_content_text(&raw_json)?;
        let json = parse_analysis_json(&content)?;
        let normalized: IntermediateAnalysis = serde_json::from_value(json.clone())
            .context("analysis JSON missing required fields")?;
        Ok(AnalysisResult {
            activity_summary: normalized.activity_summary,
            primary_application: normalized.primary_application,
            activity_category: normalized.activity_category,
            contains_sensitive: normalized.contains_sensitive.unwrap_or(false),
            contains_pii: normalized.contains_pii.unwrap_or(false),
            applications: normalized.applications.unwrap_or_default(),
            ui_elements: normalized.ui_elements.unwrap_or_default(),
            ocr_text: normalized.ocr_text,
            raw_response: raw_json,
        })
    }
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: Vec<ContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    ImageUrl { image_url: ImageInput },
}

#[derive(Serialize)]
struct ImageInput {
    url: String,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<ResponseJsonSchema>,
}

#[derive(Serialize)]
struct ResponseJsonSchema {
    name: String,
    strict: bool,
    schema: Value,
}

impl ResponseFormat {
    fn analysis_json_schema() -> Self {
        Self {
            kind: "json_schema".to_string(),
            json_schema: Some(ResponseJsonSchema {
                name: "desktop_analysis".to_string(),
                strict: true,
                schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "activity_summary",
                        "primary_application",
                        "activity_category",
                        "applications",
                        "ui_elements",
                        "contains_sensitive",
                        "contains_pii",
                        "ocr_text"
                    ],
                    "properties": {
                        "activity_summary": { "type": "string", "maxLength": 700 },
                        "primary_application": { "type": "string", "maxLength": 120 },
                        "activity_category": { "type": "string", "maxLength": 80 },
                        "applications": {
                            "type": "array",
                            "maxItems": 4,
                            "items": { "type": "string", "maxLength": 80 }
                        },
                        "ui_elements": {
                            "type": "array",
                            "maxItems": 8,
                            "items": { "type": "string", "maxLength": 120 }
                        },
                        "contains_sensitive": { "type": "boolean" },
                        "contains_pii": { "type": "boolean" },
                        "ocr_text": { "type": "string", "maxLength": 1200 }
                    }
                }),
            }),
        }
    }
}

#[derive(Deserialize)]
struct IntermediateAnalysis {
    #[serde(default)]
    activity_summary: Option<String>,
    #[serde(default)]
    primary_application: Option<String>,
    #[serde(default)]
    activity_category: Option<String>,
    #[serde(default)]
    applications: Option<Vec<String>>,
    #[serde(default)]
    ui_elements: Option<Vec<String>>,
    #[serde(default)]
    contains_sensitive: Option<bool>,
    #[serde(default)]
    contains_pii: Option<bool>,
    #[serde(default)]
    ocr_text: Option<String>,
}

fn extract_content_text(payload: &Value) -> Result<String> {
    let choices = payload
        .get("choices")
        .and_then(|v| v.as_array())
        .context("analysis response missing choices array")?;
    let first_choice = choices
        .first()
        .context("analysis response had empty choices array")?;
    let message = first_choice
        .get("message")
        .context("analysis response missing message field")?;

    // Primary path: the assistant's `content`. But some reasoning models
    // (e.g. Qwen3 via LM Studio) leave `content` empty and emit the whole
    // answer — JSON included — in `reasoning_content`. Fall back to that so the
    // analysis isn't lost just because the model "thought" its reply.
    let content = message
        .get("content")
        .map(content_to_text)
        .unwrap_or_default();
    if !content.trim().is_empty() {
        return Ok(content);
    }
    let reasoning = message
        .get("reasoning_content")
        .map(content_to_text)
        .unwrap_or_default();
    if !reasoning.trim().is_empty() {
        return Ok(reasoning);
    }

    Err(anyhow!(
        "analysis response had no content or reasoning_content"
    ))
}

/// Flatten an OpenAI-style message field that may be a plain string or an array
/// of `{type:"text", text:"…"}` parts into a single string.
fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
            .collect(),
        _ => String::new(),
    }
}

fn sanitize_json_block(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("```") {
        let mut lines: Vec<&str> = trimmed.lines().collect();
        if lines.len() >= 2 {
            lines.remove(0);
            if let Some(last) = lines.last() {
                if last.trim().starts_with("```") {
                    lines.pop();
                }
            }
        }
        return lines.join("\n").trim().to_string();
    }
    trimmed.to_string()
}

/// Drop `<think>…</think>` reasoning blocks some models emit before the answer.
/// An unclosed `<think>` (no closing tag) means the model never reached the
/// JSON, so everything from the tag onward is dropped.
fn strip_reasoning(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + "</think>".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn parse_analysis_json(raw_content: &str) -> Result<Value> {
    let cleaned = sanitize_json_block(&strip_reasoning(raw_content));
    if let Ok(value) = serde_json::from_str::<Value>(&cleaned) {
        return Ok(value);
    }

    // Reasoning models can emit a draft object inside their prose before the
    // final answer, so prefer the LAST balanced object that parses.
    for candidate in extract_balanced_objects(&cleaned).iter().rev() {
        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
            return Ok(value);
        }
    }

    Err(anyhow!("analysis content is not JSON"))
}

/// Every balanced top-level `{…}` object in `raw`, in order. Quote- and
/// escape-aware so braces inside strings don't throw off the depth count.
fn extract_balanced_objects(raw: &str) -> Vec<String> {
    let mut objects = Vec::new();
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in raw.char_indices() {
        if start.is_none() {
            if ch == '{' {
                start = Some(idx);
                depth = 1;
            }
            continue;
        }

        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start_idx) = start {
                        objects.push(raw[start_idx..=idx].to_string());
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }

    objects
}

#[cfg(test)]
mod tests {
    use super::parse_analysis_json;
    use crate::error_body::error_body_suffix;

    const FIELDS: &str = r#""activity_summary":"s","primary_application":"p","activity_category":"c","applications":[],"ui_elements":[],"contains_sensitive":false,"contains_pii":false,"ocr_text":"""#;

    #[test]
    fn a_vision_endpoint_error_leaks_neither_the_screenshot_nor_the_prompt() {
        // The message `analyze` builds on a non-2xx. LM Studio and Ollama both
        // echo the offending request, and this request is a base64 screenshot
        // plus the analysis prompt.
        let body = serde_json::json!({
            "error": {
                "message": format!(
                    "could not decode image data:image/png;base64,{} in request",
                    "iVBORw0KGgoAAAANSUhEUg".repeat(80)
                ),
                "type": "invalid_request_error",
                "param": "messages[1].content[1].image_url",
            }
        })
        .to_string();

        let msg = format!("vision endpoint error 400{}", error_body_suffix(&body));
        for secret in ["iVBORw0KGgoAAAANSUhEUg", "base64", "image_url"] {
            assert!(!msg.contains(secret), "leaked {secret:?}: {msg}");
        }
        // Status plus the endpoint's own error class is the whole diagnosis.
        assert_eq!(msg, "vision endpoint error 400 — invalid_request_error");
    }

    #[test]
    fn a_prose_only_vision_error_is_dropped_but_still_reported() {
        // Ollama's native shape: free text in `error`, nothing code-shaped.
        let body = r#"{"error":"model 'qwen3-vl' not found, try: ollama pull qwen3-vl"}"#;
        let msg = format!("vision endpoint error 404{}", error_body_suffix(body));
        assert!(!msg.contains("qwen3-vl"), "{msg}");
        assert!(msg.starts_with("vision endpoint error 404 — response body suppressed ("));
    }

    #[test]
    fn parses_plain_json() {
        let raw = format!("{{{FIELDS}}}");
        assert!(parse_analysis_json(&raw).is_ok());
    }

    #[test]
    fn strips_reasoning_block_before_json() {
        let raw = format!(
            "<think>The user seems to be coding. I'll emit {{a draft}}.</think>\n{{{FIELDS}}}"
        );
        let v = parse_analysis_json(&raw).expect("should parse JSON after </think>");
        assert_eq!(v["primary_application"], "p");
    }

    #[test]
    fn prefers_final_object_over_draft_in_prose() {
        let raw = format!("Here is a draft: {{\"foo\":1}} and the final answer:\n{{{FIELDS}}}");
        let v = parse_analysis_json(&raw).expect("should pick the last balanced object");
        assert_eq!(v["activity_category"], "c");
    }

    #[test]
    fn handles_fenced_json() {
        let raw = format!("```json\n{{{FIELDS}}}\n```");
        assert!(parse_analysis_json(&raw).is_ok());
    }

    #[test]
    fn unclosed_think_yields_error_not_panic() {
        let raw = "<think>reasoning forever with no answer and an open brace {";
        assert!(parse_analysis_json(raw).is_err());
    }

    #[test]
    fn falls_back_to_reasoning_content_when_content_empty() {
        // Mirrors Qwen3/LM Studio: empty `content`, answer in `reasoning_content`.
        let payload = serde_json::json!({
            "choices": [{ "message": { "content": "", "reasoning_content": format!("{{{FIELDS}}}") } }]
        });
        let text =
            super::extract_content_text(&payload).expect("should fall back to reasoning_content");
        let v = parse_analysis_json(&text).expect("reasoning JSON should parse");
        assert_eq!(v["primary_application"], "p");
    }
}
