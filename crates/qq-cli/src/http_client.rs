//! 基于 `reqwest` 的 HTTP 客户端适配器：替换手工 TCP/HTTP 解析，统一远程守护进程与自测请求

use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::{Client, Method, Url};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::constants::NORMAL_HTTP_TIMEOUT_SECS;

/// 统一 HTTP 响应体封装
#[derive(Debug, Clone)]
pub struct HttpResponsePayload {
    pub status_code: u16,
    pub body: String,
}

impl HttpResponsePayload {
    /// 将响应正文解析为 `serde_json::Value`
    pub fn json(&self) -> Result<Value> {
        serde_json::from_str(&self.body)
            .with_context(|| format!("failed to parse JSON response body: {}", self.body))
    }
}

/// 面向本地或远程 `adapter-server` 守护进程的复用的 HTTP 客户端封装
#[derive(Debug, Clone)]
pub struct RemoteClient {
    pub base_url: String,
    pub token: Option<String>,
    client: Client,
}

impl RemoteClient {
    /// 创建禁用系统代理的 `RemoteClient` 实例，防止本机代理环境变量劫持回环请求
    pub fn build(base_url: String, token: Option<String>) -> Result<Self> {
        let client = Client::builder()
            .no_proxy()
            .build()
            .context("failed to build reqwest HTTP client")?;
        Ok(Self {
            base_url,
            token,
            client,
        })
    }

    /// 发送带超时控制的 HTTP 请求并返回原始状态码与响应正文
    pub async fn request_with_timeout(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<HttpResponsePayload> {
        let url = join_base_and_path(&self.base_url, path);
        let mut request = self.client.request(method, &url).timeout(timeout);
        if let Some(ref bearer_token) = self.token {
            request = request.bearer_auth(bearer_token);
        }
        if let Some(json_body) = body {
            request = request.json(json_body);
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("HTTP request to {url} failed"))?;
        let status_code = response.status().as_u16();
        let response_body = response
            .text()
            .await
            .with_context(|| format!("failed reading response body from {url}"))?;
        Ok(HttpResponsePayload {
            status_code,
            body: response_body,
        })
    }

    /// 发送携带 URL 查询参数的 HTTP 请求（自动完成 UTF-8 查询字符串编码）
    pub async fn request_with_query(
        &self,
        method: Method,
        path: &str,
        query_pairs: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<HttpResponsePayload> {
        let url = join_base_and_path(&self.base_url, path);
        let mut request = self
            .client
            .request(method, &url)
            .query(query_pairs)
            .timeout(timeout);
        if let Some(ref bearer_token) = self.token {
            request = request.bearer_auth(bearer_token);
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("HTTP request to {url} failed"))?;
        let status_code = response.status().as_u16();
        let response_body = response
            .text()
            .await
            .with_context(|| format!("failed reading response body from {url}"))?;
        Ok(HttpResponsePayload {
            status_code,
            body: response_body,
        })
    }

    /// 调用远程 REST 接口并将 JSON 响应反序列化为强类型结构体
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T> {
        let method_name = method.to_string();
        let payload = self
            .request_with_timeout(
                method,
                path,
                body,
                Duration::from_secs(NORMAL_HTTP_TIMEOUT_SECS),
            )
            .await?;
        decode_checked_json(&method_name, path, payload)
    }

    /// 发送带安全编码查询参数的 GET 请求并反序列化响应 JSON
    pub async fn call_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query_pairs: &[(&str, &str)],
    ) -> Result<T> {
        let payload = self
            .request_with_query(
                Method::GET,
                path,
                query_pairs,
                Duration::from_secs(NORMAL_HTTP_TIMEOUT_SECS),
            )
            .await?;
        decode_checked_json("GET", path, payload)
    }
}

fn decode_checked_json<T: DeserializeOwned>(
    method_name: &str,
    path: &str,
    payload: HttpResponsePayload,
) -> Result<T> {
    if payload.status_code >= 400 {
        let detail = serde_json::from_str::<Value>(&payload.body)
            .ok()
            .and_then(|val| {
                val.get("error")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| payload.body.clone());
        bail!(
            "remote daemon {method_name} {path} returned HTTP {}: {detail}",
            payload.status_code
        );
    }
    serde_json::from_str(&payload.body).with_context(|| {
        format!(
            "failed to decode remote response from {method_name} {path}: {}",
            payload.body
        )
    })
}

fn join_base_and_path(base_url: &str, path: &str) -> String {
    let trimmed_base = base_url.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{trimmed_base}{path}")
    } else {
        format!("{trimmed_base}/{path}")
    }
}

/// 将监听通配地址（如 `0.0.0.0` 或 `::`）归一化为可连接的本地回环地址
pub fn normalize_connect_host(host: &str) -> &str {
    let trimmed = host.trim();
    if trimmed.is_empty() || trimmed == "0.0.0.0" || trimmed == "::" {
        "127.0.0.1"
    } else {
        trimmed
    }
}

/// 拼装含 IPv6 方括号兼容的 HTTP 基础 URL
pub fn format_base_url(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

/// 校验并规范化远程 HTTP 地址（允许省略 `http://` 前缀，显式拒绝不支持的 `https://` 等协议）
pub fn validate_http_base_url(raw_url: &str) -> Result<String> {
    let trimmed = raw_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("remote daemon URL cannot be empty");
    }
    let normalized = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    let parsed = Url::parse(&normalized)
        .with_context(|| format!("invalid remote daemon URL '{raw_url}'"))?;
    if parsed.scheme() != "http" {
        bail!(
            "unsupported remote URL scheme '{}' (only http:// is supported)",
            parsed.scheme()
        );
    }
    if parsed.host_str().is_none() {
        bail!("remote URL '{raw_url}' missing host");
    }
    Ok(normalized)
}

/// 快速探活指定地址的健康检查接口
pub async fn probe_health(
    base_url: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Result<HttpResponsePayload> {
    let client = RemoteClient::build(base_url.to_string(), token.map(ToOwned::to_owned))?;
    client
        .request_with_timeout(
            Method::GET,
            crate::constants::ROUTE_API_HEALTH,
            None,
            timeout,
        )
        .await
}

/// 在自测及工具函数场景下执行单次带基址的 JSON HTTP 请求
pub async fn http_json_request(
    base_url: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Result<HttpResponsePayload> {
    let client = RemoteClient::build(base_url.to_string(), None)?;
    client
        .request_with_timeout(
            method,
            path,
            body,
            Duration::from_secs(NORMAL_HTTP_TIMEOUT_SECS),
        )
        .await
}
