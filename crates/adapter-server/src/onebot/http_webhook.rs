//! OneBot v11 HTTP POST Event Webhook 推送器（纯 Rust RFC 3174/2104 HMAC-SHA1 签名与快速操作）

use std::fmt::Write as _;
use std::time::Duration;

use common_types::AdapterEvent;
use reqwest::Url;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::constants::{
    BEARER_PREFIX, HEADER_AUTHORIZATION, HEADER_USER_AGENT, HEADER_X_SELF_ID, HEADER_X_SIGNATURE,
    REVERSE_WS_USER_AGENT, WEBHOOK_HTTP_TIMEOUT_SECS,
};
use crate::onebot::handlers::resolve_effective_self_id;
use crate::onebot::payloads::translate_adapter_event_enriched;
use crate::onebot::quick_ops::apply_quick_operation;
use crate::onebot::reverse_ws::resolve_reverse_access_token;
use crate::AppState;

const SHA1_INIT_STATE: [u32; 5] = [
    0x6745_2301,
    0xEFCD_AB89,
    0x98BA_DCFE,
    0x1032_5476,
    0xC3D2_E1F0,
];
const HMAC_BLOCK_BYTES: usize = 64;

/// 纯 Rust 实现 RFC 3174 SHA-1 摘要算法（20 字节输出）
pub fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % HMAC_BLOCK_BYTES != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = SHA1_INIT_STATE;
    let mut w = [0u32; 80];

    let (chunks, _) = padded.as_chunks::<HMAC_BLOCK_BYTES>();
    for chunk in chunks {
        for (idx, word) in w.iter_mut().take(16).enumerate() {
            let offset = idx * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for idx in 16..80 {
            w[idx] = (w[idx - 3] ^ w[idx - 8] ^ w[idx - 14] ^ w[idx - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (round, word) in w.iter().enumerate() {
            let (f, k) = match round {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1_u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC_u32),
                _ => (b ^ c ^ d, 0xCA62_C1D6_u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (idx, word) in state.iter().enumerate() {
        out[idx * 4..idx * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// 纯 Rust 实现 RFC 2104 HMAC-SHA1 算法并返回 40 位小写十六进制签名串
pub fn hmac_sha1_hex(secret: &[u8], body: &[u8]) -> String {
    let mut key_block = [0u8; HMAC_BLOCK_BYTES];
    if secret.len() > HMAC_BLOCK_BYTES {
        let hashed = sha1_digest(secret);
        key_block[..20].copy_from_slice(&hashed);
    } else {
        key_block[..secret.len()].copy_from_slice(secret);
    }

    let mut inner = Vec::with_capacity(HMAC_BLOCK_BYTES + body.len());
    for byte in key_block {
        inner.push(byte ^ 0x36);
    }
    inner.extend_from_slice(body);
    let inner_hash = sha1_digest(&inner);

    let mut outer = Vec::with_capacity(HMAC_BLOCK_BYTES + 20);
    for byte in key_block {
        outer.push(byte ^ 0x5c);
    }
    outer.extend_from_slice(&inner_hash);
    let digest = sha1_digest(&outer);

    let mut hex = String::with_capacity(40);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn is_loopback_url(target_url: &str) -> bool {
    let Ok(parsed) = Url::parse(target_url) else {
        return false;
    };
    matches!(
        parsed.host_str(),
        Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
    )
}

fn build_webhook_client(target_url: &str) -> Option<reqwest::Client> {
    let mut builder =
        reqwest::Client::builder().timeout(Duration::from_secs(WEBHOOK_HTTP_TIMEOUT_SECS));
    if is_loopback_url(target_url) {
        builder = builder.no_proxy();
    }
    match builder.build() {
        Ok(client) => Some(client),
        Err(err) => {
            tracing::error!(url = %target_url, ?err, "构建 OneBot HTTP Webhook 客户端失败");
            None
        }
    }
}

async fn post_single_event_and_quick_op(
    state: &AppState,
    client: &reqwest::Client,
    target_url: &str,
    self_id: i64,
    event_json: &Value,
) {
    let Ok(body_bytes) = serde_json::to_vec(event_json) else {
        return;
    };

    let mut req = client
        .post(target_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(HEADER_X_SELF_ID, self_id.to_string())
        .header(HEADER_USER_AGENT, REVERSE_WS_USER_AGENT);

    if let Some(token) = resolve_reverse_access_token(&state.config) {
        req = req.header(HEADER_AUTHORIZATION, format!("{BEARER_PREFIX}{token}"));
    }

    if let Some(secret) = state
        .config
        .onebot
        .secret
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
    {
        let sig = hmac_sha1_hex(secret.as_bytes(), &body_bytes);
        req = req.header(HEADER_X_SIGNATURE, format!("sha1={sig}"));
    }

    let send_res = tokio::select! {
        _ = state.wait_shutdown() => return,
        res = req.body(body_bytes).send() => res,
    };

    let response = match send_res {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(url = %target_url, ?err, "OneBot HTTP Event Webhook 推送失败");
            return;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            url = %target_url,
            status = %response.status(),
            "OneBot HTTP Event Webhook 服务端返回非成功状态"
        );
        return;
    }

    let Ok(resp_bytes) = response.bytes().await else {
        return;
    };
    if resp_bytes.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    let Ok(Value::Object(op_map)) = serde_json::from_slice::<Value>(&resp_bytes) else {
        return;
    };
    if op_map.is_empty() || state.is_shutting_down() {
        return;
    }

    if let Err(err) = apply_quick_operation(state, event_json, &Value::Object(op_map)).await {
        tracing::warn!(
            url = %target_url,
            error = %err,
            "执行 HTTP Webhook 返回的快速操作失败"
        );
    }
}

async fn run_http_post_loop(state: AppState, target_url: String) {
    let Some(client) = build_webhook_client(&target_url) else {
        return;
    };
    let mut event_receiver = state.driver.subscribe_events();

    loop {
        let recv_res = tokio::select! {
            _ = state.wait_shutdown() => break,
            res = event_receiver.recv() => res,
        };

        match recv_res {
            Ok(
                event @ (AdapterEvent::GroupMessage(_)
                | AdapterEvent::PrivateMessage(_)
                | AdapterEvent::MessageSent(_)
                | AdapterEvent::Notice(_)
                | AdapterEvent::Request(_)),
            ) => {
                let current_self_id = resolve_effective_self_id(&state).await;
                let Some(event_json) =
                    translate_adapter_event_enriched(&state, current_self_id, &event).await
                else {
                    continue;
                };
                post_single_event_and_quick_op(
                    &state,
                    &client,
                    &target_url,
                    current_self_id,
                    &event_json,
                )
                .await;
            }
            Ok(_) => {}
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, url = %target_url, "OneBot HTTP Webhook 事件接收器发生积压丢弃");
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// 为配置中的每个 HTTP POST Event 上报地址启动独立异步推送任务
pub fn spawn_http_post_webhooks(state: AppState) -> Vec<JoinHandle<()>> {
    state
        .config
        .onebot
        .http_post_urls
        .iter()
        .map(|raw| raw.trim())
        .filter(|url| !url.is_empty())
        .map(|url| {
            let task_state = state.clone();
            let owned_url = url.to_string();
            tokio::spawn(async move {
                run_http_post_loop(task_state, owned_url).await;
            })
        })
        .collect()
}
