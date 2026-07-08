//! AG-UI event types + the Personal Agent bus envelope decoder.
//!
//! The backend streams `BusRecord { v, run_id, seq, ev }` over SSE where `ev` is an
//! AG-UI event (Frozen Contract #3/#4). We consume AG-UI exclusively. This mirrors
//! the web client's `apps/web/src/services/agui.ts` and the contract definitions in
//! `packages/personal-agent-contracts/.../ag_ui.py`.

use serde::Deserialize;

// Standard AG-UI event type names we react to.
#[allow(dead_code)] // part of the protocol; not yet surfaced in the UI
pub const RUN_STARTED: &str = "RUN_STARTED";
pub const RUN_FINISHED: &str = "RUN_FINISHED";
pub const RUN_ERROR: &str = "RUN_ERROR";
pub const TEXT_MESSAGE_CONTENT: &str = "TEXT_MESSAGE_CONTENT";
pub const THINKING_CONTENT: &str = "THINKING_TEXT_MESSAGE_CONTENT";
pub const TOOL_CALL_START: &str = "TOOL_CALL_START";
pub const TOOL_CALL_ARGS: &str = "TOOL_CALL_ARGS";
pub const TOOL_CALL_RESULT: &str = "TOOL_CALL_RESULT";
pub const CUSTOM: &str = "CUSTOM";

// Namespaced personal_agent CUSTOM event names.
pub const CUSTOM_USAGE: &str = "personal_agent.usage";

/// One AG-UI event. Fields are optional because each event type populates only a subset
/// (e.g. text deltas use `delta`, tool calls use `tool_call_id`/`tool_call_name`).
#[allow(dead_code)] // some fields are decoded for completeness but not yet rendered
#[derive(Deserialize, Debug, Default)]
pub struct AgUiEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub delta: Option<String>,
    #[serde(default, alias = "toolCallId")]
    pub tool_call_id: Option<String>,
    #[serde(default, alias = "toolCallName")]
    pub tool_call_name: Option<String>,
    /// Tool result body (TOOL_CALL_RESULT).
    #[serde(default)]
    pub content: Option<String>,
    /// RUN_ERROR human-readable message.
    #[serde(default)]
    pub message: Option<String>,
    /// CUSTOM event name (e.g. `personal_agent.usage`).
    #[serde(default)]
    pub name: Option<String>,
    /// CUSTOM event payload.
    #[serde(default)]
    pub value: Option<serde_json::Value>,
}

/// The versioned wire record. The client de-dupes on `(run_id, seq)` since publishes
/// are at-least-once.
#[allow(dead_code)] // run_id/seq decoded for completeness; dedup is a later slice
#[derive(Deserialize, Debug)]
pub struct BusRecord {
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub seq: i64,
    pub ev: AgUiEvent,
}

pub fn parse_bus_record(data: &str) -> Option<BusRecord> {
    serde_json::from_str(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_text_delta_frame() {
        let data = r#"{"v":1,"run_id":"run:abc","seq":3,"ev":{"type":"TEXT_MESSAGE_CONTENT","delta":"Hallo"}}"#;
        let rec = parse_bus_record(data).expect("parses");
        assert_eq!(rec.run_id, "run:abc");
        assert_eq!(rec.seq, 3);
        assert_eq!(rec.ev.kind, TEXT_MESSAGE_CONTENT);
        assert_eq!(rec.ev.delta.as_deref(), Some("Hallo"));
    }

    #[test]
    fn decodes_camelcase_tool_fields() {
        // The wire uses AG-UI's camelCase tool keys; our aliases must accept them.
        let data = r#"{"run_id":"r","seq":0,"ev":{"type":"TOOL_CALL_START","toolCallId":"t1","toolCallName":"web_search"}}"#;
        let rec = parse_bus_record(data).expect("parses");
        assert_eq!(rec.ev.tool_call_id.as_deref(), Some("t1"));
        assert_eq!(rec.ev.tool_call_name.as_deref(), Some("web_search"));
    }

    #[test]
    fn decodes_custom_usage_value() {
        let data = r#"{"run_id":"r","seq":0,"ev":{"type":"CUSTOM","name":"personal_agent.usage","value":{"model_name":"x","input_tokens":10,"output_tokens":5}}}"#;
        let rec = parse_bus_record(data).expect("parses");
        assert_eq!(rec.ev.kind, CUSTOM);
        assert_eq!(rec.ev.name.as_deref(), Some(CUSTOM_USAGE));
        let v = rec.ev.value.unwrap();
        assert_eq!(v["input_tokens"].as_i64(), Some(10));
    }
}
