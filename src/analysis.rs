use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::AnalysisConfig;

const ANALYSIS_MAX_TOKENS: u32 = 1_800;

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
            return Err(anyhow!("vision endpoint error {status}: {body}"));
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
    let content = message
        .get("content")
        .context("analysis response missing content field")?;

    match content {
        Value::String(s) => Ok(s.clone()),
        Value::Array(parts) => {
            let mut buffer = String::new();
            for part in parts {
                if part
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(|kind| kind == "text")
                    .unwrap_or(false)
                {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        buffer.push_str(text);
                    }
                }
            }
            if buffer.is_empty() {
                Err(anyhow!("analysis content array had no text entries"))
            } else {
                Ok(buffer)
            }
        }
        _ => Err(anyhow!("analysis content format unsupported")),
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

fn parse_analysis_json(raw_content: &str) -> Result<Value> {
    let cleaned = sanitize_json_block(raw_content);
    if let Ok(value) = serde_json::from_str::<Value>(&cleaned) {
        return Ok(value);
    }

    if let Some(candidate) = extract_first_json_object(&cleaned) {
        if let Ok(value) = serde_json::from_str::<Value>(&candidate) {
            return Ok(value);
        }
    }

    Err(anyhow!("analysis content is not JSON"))
}

fn extract_first_json_object(raw: &str) -> Option<String> {
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
                        return Some(raw[start_idx..=idx].to_string());
                    }
                }
            }
            _ => {}
        }
    }

    None
}
