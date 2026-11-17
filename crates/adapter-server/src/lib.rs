//! 适配服务门面模块（Facade）：组装 Axum 路由、全局鉴权中间件、反向 WS 任务与启停生命周期

pub mod auth;
pub mod constants;
pub mod error;
pub mod onebot;
pub mod rest;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use common_types::{AdapterConfig, BotDriver};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::auth::bearer_auth_middleware;
use crate::constants::{
    DEFAULT_RUNTIME_DIR, HTTP_MAX_BODY_BYTES, MAX_INFLIGHT_ASYNC_ACTIONS, ROUTE_API_AUDIT_EVENTS,
    ROUTE_API_CACHE_CLEAN, ROUTE_API_DELETE_MSG, ROUTE_API_DIAG, ROUTE_API_DIAG_PROBE_UI,
    ROUTE_API_ENSURE_QQ, ROUTE_API_FORWARD_MSG, ROUTE_API_FRIENDS, ROUTE_API_GROUPS,
    ROUTE_API_GROUP_INFO, ROUTE_API_GROUP_MEMBERS, ROUTE_API_HEALTH, ROUTE_API_MESSAGES,
    ROUTE_API_MESSAGES_HISTORY, ROUTE_API_OCR, ROUTE_API_OPEN_GROUP, ROUTE_API_OPEN_PRIVATE,
    ROUTE_API_RESTART, ROUTE_API_SCHEDULER, ROUTE_API_SCHEDULER_POLICY, ROUTE_API_SCREENSHOT,
    ROUTE_API_SECURITY_RESET, ROUTE_API_SEND, ROUTE_API_SESSIONS, ROUTE_API_SESSIONS_READ,
    ROUTE_API_SHUTDOWN, ROUTE_API_STATUS, ROUTE_API_STORAGE, ROUTE_API_STORAGE_STATS,
    ROUTE_API_TREE, ROUTE_API_WINDOWS, ROUTE_ONEBOT_API_WS, ROUTE_ONEBOT_EVENT_WS,
    ROUTE_ONEBOT_ROOT_EVENT_WS, ROUTE_ONEBOT_WS, ROUTE_ONEBOT_WS_API, ROUTE_ONEBOT_WS_EVENT,
    ROUTE_ONEBOT_WS_ROOT, ROUTE_WS_EVENTS, RUNTIME_CONNECTION_FILE,
};

/// 注入至所有 Axum 处理器、中间件与反向 WS 任务的共享服务状态
#[derive(Clone)]
pub struct AppState {
    pub driver: Arc<dyn BotDriver>,
    pub config: AdapterConfig,
    pub shutdown_notify: Arc<Notify>,
    pub shutting_down: Arc<AtomicBool>,
    pub send_gate: Arc<tokio::sync::Mutex<()>>,
    pub rate_limit_gate: Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>>,
    pub async_action_semaphore: Arc<tokio::sync::Semaphore>,
    pub restart_in_progress: Arc<AtomicBool>,
    pub last_message_time_secs: Arc<AtomicI64>,
    pub ws_disconnect_times: Arc<AtomicU64>,
}

impl AppState {
    /// 构造默认服务共享状态，初始化停机标志、发送串行互斥锁、限速时间戳、异步排队信号量与防重入重启门闩
    pub fn new(driver: Arc<dyn BotDriver>, config: AdapterConfig) -> Self {
        Self {
            driver,
            config,
            shutdown_notify: Arc::new(Notify::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            send_gate: Arc::new(tokio::sync::Mutex::new(())),
            rate_limit_gate: Arc::new(tokio::sync::Mutex::new(None)),
            async_action_semaphore: Arc::new(tokio::sync::Semaphore::new(
                MAX_INFLIGHT_ASYNC_ACTIONS,
            )),
            restart_in_progress: Arc::new(AtomicBool::new(false)),
            last_message_time_secs: Arc::new(AtomicI64::new(0)),
            ws_disconnect_times: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 设置优雅停机标志并唤醒所有正在等待停机的异步任务
    pub fn trigger_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.shutdown_notify.notify_waiters();
    }

    /// 判断服务是否已收到优雅停机信号
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// 记录最近一次收到或发出消息的 Unix 秒级时间戳，用于 OneBot `get_status` 与心跳 `stat` 上报
    pub fn record_message_activity(&self) {
        self.last_message_time_secs
            .store(chrono::Utc::now().timestamp(), Ordering::Release);
    }

    /// 读取最近一次消息活动 Unix 秒级时间戳（尚未发生消息活动时返回 `0`）
    pub fn last_message_time_secs(&self) -> i64 {
        self.last_message_time_secs.load(Ordering::Acquire)
    }

    /// 递增 WebSocket 断连计数，供心跳及状态接口展示链路稳定性指标
    pub fn record_ws_disconnect(&self) {
        self.ws_disconnect_times.fetch_add(1, Ordering::Relaxed);
    }

    /// 读取累计 WebSocket 断连次数
    pub fn ws_disconnect_times(&self) -> u64 {
        self.ws_disconnect_times.load(Ordering::Relaxed)
    }

    /// 异步等待停机信号；通过先注册等待器再二次核验标志避免信号提前触发造成丢失唤醒
    pub async fn wait_shutdown(&self) {
        if self.is_shutting_down() {
            return;
        }
        let notified = self.shutdown_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_shutting_down() {
            return;
        }
        notified.await;
    }
}

/// 后台任务生命周期守卫：当服务任务结束或被中止时统一清理关联反向 WS 协程
struct ReverseTaskGuard(Vec<JoinHandle<()>>);

impl Drop for ReverseTaskGuard {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

/// 基于现有 `AppState` 构建完整 Axum 路由（含原生 REST/WS 与 OneBot v11 接口）
pub fn build_router_with_state(state: AppState) -> Router {
    Router::new()
        .route(ROUTE_API_HEALTH, get(rest::health))
        .route(ROUTE_API_STATUS, get(rest::status))
        .route(ROUTE_API_WINDOWS, get(rest::windows))
        .route(ROUTE_API_ENSURE_QQ, post(rest::ensure_qq))
        .route(ROUTE_API_DIAG, get(rest::diag))
        .route(ROUTE_API_DIAG_PROBE_UI, post(rest::probe_ui))
        .route(ROUTE_API_SCREENSHOT, get(rest::screenshot))
        .route(ROUTE_API_SHUTDOWN, post(rest::shutdown))
        .route(ROUTE_API_RESTART, post(rest::restart))
        .route(ROUTE_API_TREE, get(rest::tree))
        .route(ROUTE_API_SESSIONS, get(rest::sessions))
        .route(ROUTE_API_MESSAGES, get(rest::messages))
        .route(ROUTE_API_MESSAGES_HISTORY, get(rest::messages_history))
        .route(ROUTE_API_OPEN_GROUP, post(rest::open_group))
        .route(ROUTE_API_OPEN_PRIVATE, post(rest::open_private))
        .route(ROUTE_API_OCR, post(rest::ocr))
        .route(ROUTE_API_DELETE_MSG, post(rest::delete_msg))
        .route(ROUTE_API_SEND, post(rest::send))
        .route(ROUTE_API_FRIENDS, get(rest::friends))
        .route(ROUTE_API_GROUPS, get(rest::groups))
        .route(ROUTE_API_GROUP_INFO, get(rest::group_info))
        .route(ROUTE_API_GROUP_MEMBERS, get(rest::group_members))
        .route(ROUTE_API_STORAGE_STATS, get(rest::storage_stats))
        .route(ROUTE_API_STORAGE, get(rest::storage_overview))
        .route(ROUTE_API_SCHEDULER, get(rest::get_scheduler))
        .route(
            ROUTE_API_SCHEDULER_POLICY,
            get(rest::get_scheduler_policy).post(rest::update_scheduler_policy),
        )
        .route(ROUTE_API_SESSIONS_READ, post(rest::mark_session_read))
        .route(ROUTE_API_FORWARD_MSG, post(rest::forward_msg))
        .route(ROUTE_API_AUDIT_EVENTS, get(rest::audit_events))
        .route(ROUTE_API_SECURITY_RESET, post(rest::reset_security))
        .route(ROUTE_API_CACHE_CLEAN, post(rest::clean_cache))
        .route(ROUTE_WS_EVENTS, get(rest::ws_events))
        .route(ROUTE_ONEBOT_WS, get(onebot::ws_onebot))
        .route(ROUTE_ONEBOT_WS_ROOT, get(onebot::ws_onebot))
        .route(ROUTE_ONEBOT_WS_API, get(onebot::ws_onebot_api))
        .route(ROUTE_ONEBOT_API_WS, get(onebot::ws_onebot_api))
        .route(ROUTE_ONEBOT_WS_EVENT, get(onebot::ws_onebot_event))
        .route(ROUTE_ONEBOT_EVENT_WS, get(onebot::ws_onebot_event))
        .route(ROUTE_ONEBOT_ROOT_EVENT_WS, get(onebot::ws_onebot_event))
        .merge(onebot::onebot_routes())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            bearer_auth_middleware,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(HTTP_MAX_BODY_BYTES))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// 构造默认运行所需的 Axum 路由器
pub fn build_router(driver: Arc<dyn BotDriver>, config: AdapterConfig) -> Router {
    build_router_with_state(AppState::new(driver, config))
}

/// 将实际监听地址、端口与进程 PID 写入 `.runtime/connection.json`
pub async fn write_connection_info(listen_addr: SocketAddr) -> anyhow::Result<()> {
    let runtime_dir = Path::new(DEFAULT_RUNTIME_DIR);
    tokio::fs::create_dir_all(runtime_dir).await?;
    let payload = json!({
        "host": listen_addr.ip().to_string(),
        "port": listen_addr.port(),
        "pid": std::process::id(),
    });
    tokio::fs::write(
        Path::new(RUNTIME_CONNECTION_FILE),
        serde_json::to_vec_pretty(&payload)?,
    )
    .await?;
    Ok(())
}

async fn shutdown_signal(state: AppState) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl+C received, stopping adapter-server");
            state.trigger_shutdown();
        }
        _ = state.wait_shutdown() => {
            tracing::info!("HTTP shutdown notification received, stopping adapter-server");
        }
    }
}

fn spawn_background_onebot_tasks(state: &AppState) -> ReverseTaskGuard {
    let mut handles = onebot::spawn_reverse_ws_clients(state.clone());
    handles.extend(onebot::spawn_http_post_webhooks(state.clone()));
    ReverseTaskGuard(handles)
}

/// 绑定配置监听地址，写出 `.runtime/connection.json`，启动反向 WS 与 Webhook 并阻塞服务直至停机
pub async fn bind_and_serve(
    driver: Arc<dyn BotDriver>,
    config: AdapterConfig,
) -> anyhow::Result<()> {
    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&bind_addr).await?;
    let local_addr = listener.local_addr()?;

    if let Err(err) = write_connection_info(local_addr).await {
        tracing::warn!(?err, "failed to write connection metadata");
    }

    tracing::info!(%local_addr, "adapter-server listening");
    let state = AppState::new(driver, config);
    let _reverse_guard = spawn_background_onebot_tasks(&state);
    let app = build_router_with_state(state.clone());

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state))
        .await?;
    Ok(())
}

/// 绑定配置地址（支持 `:0` 临时端口）并在后台任务中运行服务及反向 WS / Webhook 任务
pub async fn spawn_server(
    driver: Arc<dyn BotDriver>,
    config: AdapterConfig,
) -> anyhow::Result<(SocketAddr, JoinHandle<()>)> {
    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&bind_addr).await?;
    let local_addr = listener.local_addr()?;
    let state = AppState::new(driver, config);
    let reverse_guard = spawn_background_onebot_tasks(&state);
    let app = build_router_with_state(state.clone());

    let handle = tokio::spawn(async move {
        let _reverse_guard = reverse_guard;
        if let Err(err) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal(state))
            .await
        {
            tracing::error!(?err, "adapter-server exited with error");
        }
    });

    Ok((local_addr, handle))
}
