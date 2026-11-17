//! 13 大 QA 测试类别统一注册调度中心：基于声明式宏汇总 L0-L3 测试套件、隔离环境夹具与异步执行器（Macro Codegen + Facade）

pub mod concurrency_and_integration;
pub mod data_and_cond_boundary;
pub mod flood_and_onebot;
pub mod perf_mem_cpu;
pub mod protocol_v4_suite;
pub mod smoke_and_stub;
pub mod stress_and_bruteforce;
pub mod stub_driver;
pub mod wave6_suite;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use common_types::AdapterConfig;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use reqwest::Client;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::constants::HTTP_POOL_MAX_IDLE_PER_HOST;
use crate::models::{CaseOutcome, CategoryReport, ProcessTelemetrySnapshot, QaTier};
use stub_driver::ConfigurableStubDriver;

/// 测试套件运行期共享上下文（Parameter Object）
pub struct SuiteExecContext {
    pub live_url: String,
    pub skip_live: bool,
    pub http_client: Client,
    pub live_telemetry: Mutex<Option<ProcessTelemetrySnapshot>>,
}

impl SuiteExecContext {
    /// 初始化无系统代理、支持并发连接池复用的测试上下文
    pub fn new(live_url: String, skip_live: bool) -> Result<Self> {
        Ok(Self {
            live_url,
            skip_live,
            http_client: build_no_proxy_client()?,
            live_telemetry: Mutex::new(None),
        })
    }
}

/// 单个 QA 测试类别结果构建器（Builder）
pub struct CategoryBuilder {
    id: &'static str,
    category_zh: &'static str,
    category_en: &'static str,
    tier: QaTier,
    sla_target: &'static str,
    started: Instant,
    key_metrics: BTreeMap<String, f64>,
    cases: Vec<CaseOutcome>,
    sla_passed: bool,
    sla_actual: String,
}

impl CategoryBuilder {
    /// 创建指定分类与 SLA 目标的报告构建器
    pub fn new(
        id: &'static str,
        category_zh: &'static str,
        category_en: &'static str,
        tier: QaTier,
        sla_target: &'static str,
    ) -> Self {
        Self {
            id,
            category_zh,
            category_en,
            tier,
            sla_target,
            started: Instant::now(),
            key_metrics: BTreeMap::new(),
            cases: Vec::new(),
            sla_passed: true,
            sla_actual: "全部断言达标".to_string(),
        }
    }

    /// 异步执行单条用例并捕获断言或异常详情，确保单点失败不会阻断后续诊断用例采集
    pub async fn run_case<Fut>(&mut self, name: &str, fut: Fut)
    where
        Fut: Future<Output = Result<String>>,
    {
        let t0 = Instant::now();
        let outcome = match fut.await {
            Ok(detail) => CaseOutcome {
                name: name.to_string(),
                passed: true,
                skipped: false,
                duration_ms: t0.elapsed().as_secs_f64() * 1000.0,
                detail,
            },
            Err(err) => CaseOutcome {
                name: name.to_string(),
                passed: false,
                skipped: false,
                duration_ms: t0.elapsed().as_secs_f64() * 1000.0,
                detail: format!("{err:#}"),
            },
        };
        self.cases.push(outcome);
    }

    /// 记录一条已跳过并附带明确原因的受控用例（跳过项不计入通过数）
    pub fn push_skipped(&mut self, name: &str, reason: &str) {
        self.cases.push(CaseOutcome {
            name: name.to_string(),
            passed: false,
            skipped: true,
            duration_ms: 0.0,
            detail: reason.to_string(),
        });
    }

    /// 记录分类级量化监控指标
    pub fn set_metric(&mut self, key: &str, value: f64) {
        self.key_metrics.insert(key.to_string(), value);
    }

    /// 写入分类级 SLA 达标判定结果与实测说明
    pub fn set_sla(&mut self, passed: bool, actual: String) {
        self.sla_passed = passed;
        self.sla_actual = actual;
    }

    /// 汇总生成最终 `CategoryReport`
    pub fn build(self) -> CategoryReport {
        let total_cases = self.cases.len();
        let passed_cases = self.cases.iter().filter(|c| c.passed && !c.skipped).count();
        let skipped_cases = self.cases.iter().filter(|c| c.skipped).count();
        let failed_cases = self
            .cases
            .iter()
            .filter(|c| !c.passed && !c.skipped)
            .count();
        let passed = failed_cases == 0 && self.sla_passed;
        // 若用例断言失败但未显式覆盖 SLA 文案，自动替换为明确失败统计以防误报
        let sla_actual = if failed_cases > 0 && self.sla_passed {
            format!("{failed_cases}/{total_cases} 项用例断言失败")
        } else {
            self.sla_actual
        };
        CategoryReport {
            id: self.id.to_string(),
            category_zh: self.category_zh.to_string(),
            category_en: self.category_en.to_string(),
            tier: self.tier.as_str().to_string(),
            passed,
            total_cases,
            passed_cases,
            skipped_cases,
            failed_cases,
            duration_ms: self.started.elapsed().as_secs_f64() * 1000.0,
            sla_target: self.sla_target.to_string(),
            sla_actual,
            key_metrics: self.key_metrics,
            cases: self.cases,
        }
    }
}

/// 构造显式禁用系统代理并支持高并发连接复用的回环 HTTP 客户端
pub fn build_no_proxy_client() -> Result<Client> {
    Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(HTTP_POOL_MAX_IDLE_PER_HOST)
        .timeout(Duration::from_secs(10))
        .build()
        .context("build no_proxy reqwest client")
}

/// 构造标准隔离配置与可控测试桩驱动对（固定 `:0` 临时端口与 `:memory:` 库）
pub fn default_stub_and_config() -> (Arc<ConfigurableStubDriver>, AdapterConfig) {
    let driver = ConfigurableStubDriver::new();
    let mut config = AdapterConfig::default();
    config.server.host = "127.0.0.1".to_string();
    config.server.port = 0;
    config.storage.db_path = ":memory:".to_string();
    config.target.allowed_private_friends = vec!["测试好友".to_string()];
    config.onebot.heartbeat_interval_ms = 60_000;
    config.onebot.reconnect_interval_ms = 200;
    (driver, config)
}

/// 发送 JSON POST 请求并返回 HTTP 状态码与解析后的 JSON 响应体
pub(super) async fn post_json_status(
    client: &Client,
    url: &str,
    payload: Value,
) -> Result<(u16, Value)> {
    let resp = client.post(url).json(&payload).send().await?;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await?;
    Ok((status, body))
}

/// 发送 GET 请求并返回 HTTP 状态码与解析后的 JSON 响应体
pub(super) async fn get_json_status(client: &Client, url: &str) -> Result<(u16, Value)> {
    let resp = client.get(url).send().await?;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await?;
    Ok((status, body))
}

/// 在超时窗口内从正/反向 WebSocket 流读取下一帧 JSON（自动响应底层 Ping 控制帧）
pub async fn ws_next_json<S>(ws: &mut WebSocketStream<S>, timeout: Duration) -> Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for WebSocket JSON frame");
        }
        let frame = tokio::time::timeout(remaining, ws.next())
            .await
            .context("WebSocket read timeout")?
            .ok_or_else(|| anyhow::anyhow!("WebSocket stream ended unexpectedly"))??;
        match frame {
            Message::Text(text) => {
                let val: Value = serde_json::from_str(text.as_str())
                    .context("parse WebSocket text frame as JSON")?;
                return Ok(val);
            }
            Message::Ping(payload) => {
                ws.send(Message::Pong(payload)).await?;
            }
            Message::Close(_) => bail!("WebSocket closed by peer"),
            _ => {}
        }
    }
}

macro_rules! define_qa_suite_registry {
    ($(($tier:expr, $runner:path)),* $(,)?) => {
        /// 按所选等级依次执行对应的 QA 测试类别并收集报告列表（Macro Codegen）
        pub async fn run_all_categories(tier: QaTier, ctx: &SuiteExecContext) -> Vec<CategoryReport> {
            let mut reports = Vec::new();
            $(
                if tier.includes($tier) {
                    reports.push($runner(ctx).await);
                }
            )*
            reports
        }
    };
}

define_qa_suite_registry! {
    (QaTier::L0, smoke_and_stub::run_smoke_category),
    (QaTier::L1, smoke_and_stub::run_stub_category),
    (QaTier::L1, data_and_cond_boundary::run_data_boundary_category),
    (QaTier::L1, data_and_cond_boundary::run_condition_boundary_category),
    (QaTier::L1, protocol_v4_suite::run_protocol_v4_category),
    (QaTier::L1, wave6_suite::run_wave6_category),
    (QaTier::L2, concurrency_and_integration::run_concurrency_category),
    (QaTier::L2, concurrency_and_integration::run_integration_category),
    (QaTier::L3, stress_and_bruteforce::run_stress_category),
    (QaTier::L3, stress_and_bruteforce::run_bruteforce_category),
    (QaTier::L3, perf_mem_cpu::run_performance_category),
    (QaTier::L3, perf_mem_cpu::run_memory_load_category),
    (QaTier::L3, perf_mem_cpu::run_cpu_load_category),
}
