//! OneBot v11 协议模块门面：聚合宏生成路由、统一动作分发器、正反向多角色 WS 及 HTTP Webhook

#[macro_use]
pub mod macros;
pub mod extended_handlers;
pub mod file_ocr_handlers;
pub mod forward_ws;
pub mod handlers;
pub mod http_webhook;
pub mod payloads;
pub mod poke_recall_handlers;
pub mod quick_ops;
pub mod reverse_ws;
pub mod wave6_handlers;
pub mod ws_runner;

pub use forward_ws::{ws_onebot, ws_onebot_api, ws_onebot_event};
pub use handlers::{
    dispatch_onebot_action, dispatch_onebot_action_with_mode, execute_onebot_ws_envelope,
    onebot_routes, ActionDispatchOutcome,
};
pub use http_webhook::{hmac_sha1_hex, spawn_http_post_webhooks};
pub use payloads::{
    chat_message_to_onebot_event, extract_onebot_message_text, extract_onebot_segments,
    resolve_effective_deadline_ms, segments_to_onebot_json, stored_record_to_onebot_msg,
    translate_adapter_event_to_onebot, OneBotSendPayload,
};
pub use reverse_ws::spawn_reverse_ws_clients;
