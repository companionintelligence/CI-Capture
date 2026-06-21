use anyhow::Result;
use serde::Serialize;

use crate::observation::Observation;

#[derive(Debug, Clone, Serialize)]
pub struct DiffEnvelope {
    pub ts: f64,
    pub monotonic_ms: u64,
    pub patch: json_patch::Patch,
}

pub fn create_diff_envelope(previous: &Observation, current: &Observation) -> Result<DiffEnvelope> {
    let previous_value = serde_json::to_value(previous)?;
    let current_value = serde_json::to_value(current)?;

    let patch = json_patch::diff(&previous_value, &current_value);

    Ok(DiffEnvelope {
        ts: current.ts,
        monotonic_ms: current.monotonic_ms,
        patch,
    })
}
