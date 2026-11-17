//! OneBot v11 消息撤回标准动作处理器（`delete_msg`）

use common_types::AdapterError;
use serde_json::{json, Value};

use crate::onebot::payloads::parse_value_as_i64;
use crate::AppState;

fn parse_required_positive_i64(
    params: &Value,
    keys: &[&str],
    missing_label: &str,
) -> std::result::Result<i64, AdapterError> {
    for &key in keys {
        if let Some(raw) = params.get(key) {
            if let Some(parsed) = parse_value_as_i64(raw, key)? {
                if parsed <= 0 {
                    return Err(AdapterError::InputRejected(format!(
                        "'{key}' must be a positive integer"
                    )));
                }
                return Ok(parsed);
            }
        }
    }
    Err(AdapterError::InputRejected(format!(
        "{missing_label} is required"
    )))
}

/// `delete_msg` -> 在真实 QQNT 客户端通过右键“撤回”撤回消息并清理持久化记录
pub(crate) async fn exec_delete_msg(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, AdapterError> {
    let message_id = parse_required_positive_i64(&params, &["message_id"], "message_id")?;
    let _send_guard = state.send_gate.lock().await;
    state.driver.delete_message(message_id).await?;
    Ok(json!({}))
}
