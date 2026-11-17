//! OneBot v11 声明式宏代码生成模块（Macro Codegen）：通过单表同步生成统一动作分发器与动态路由

/// 根据声明式动作配置表（受支持动作与显式未支持动作）自动生成路由与分发器
macro_rules! define_onebot_actions {
    (
        supported {
            $(
                $action_const:path => {
                    method: $method_kind:ident,
                    target: $impl_fn:path $(,)?
                }
            ),* $(,)?
        }
        unsupported {
            $(
                $unsupported_action:expr => $unsupported_reason:expr
            ),* $(,)?
        } $(,)?
    ) => {
        /// OneBot v11 单次动作执行结果分类（区分同步应答数据与异步受理状态）
        pub enum ActionDispatchOutcome {
            Sync(serde_json::Value),
            AsyncAccepted,
        }

        /// OneBot v11 动作在宏生成配置表中的静态元信息分类
        enum RegisteredActionKind {
            Supported {
                base_action: &'static str,
                post_only: bool,
                is_async: bool,
                rate_limited: bool,
            },
            Unsupported(&'static str),
            Unknown,
        }

        fn split_action_modifiers(action: &str) -> (&str, bool, bool) {
            let mut base = action;
            let mut rate_limited = false;
            let mut is_async = false;
            if let Some(stripped) = base.strip_suffix("_rate_limited") {
                base = stripped;
                rate_limited = true;
            }
            if let Some(stripped) = base.strip_suffix("_async") {
                base = stripped;
                is_async = true;
            }
            if !rate_limited {
                if let Some(stripped) = base.strip_suffix("_rate_limited") {
                    base = stripped;
                    rate_limited = true;
                }
            }
            (base, is_async || rate_limited, rate_limited)
        }

        fn lookup_registered_action(raw_action: &str) -> RegisteredActionKind {
            let (base, is_async, rate_limited) = split_action_modifiers(raw_action.trim());
            $(
                if base == $action_const {
                    let post_only = define_onebot_actions!(@is_post_only $method_kind);
                    return RegisteredActionKind::Supported {
                        base_action: $action_const,
                        post_only,
                        is_async,
                        rate_limited,
                    };
                }
            )*
            $(
                if base == $unsupported_action {
                    return RegisteredActionKind::Unsupported($unsupported_reason);
                }
            )*
            RegisteredActionKind::Unknown
        }

        async fn dispatch_base_action(
            state: &crate::AppState,
            base_action: &str,
            params: serde_json::Value,
        ) -> std::result::Result<serde_json::Value, common_types::AdapterError> {
            $(
                if base_action == $action_const {
                    return $impl_fn(state, params).await;
                }
            )*
            Err(common_types::AdapterError::ActionNotFound {
                action: base_action.to_string(),
            })
        }

        /// 执行支持前缀与 `_async` / `_rate_limited` 后缀修饰符的 OneBot v11 动作，返回同步数据或异步受理
        pub async fn dispatch_onebot_action_with_mode(
            state: &crate::AppState,
            action: &str,
            params: serde_json::Value,
        ) -> std::result::Result<ActionDispatchOutcome, common_types::AdapterError> {
            match lookup_registered_action(action) {
                RegisteredActionKind::Supported {
                    base_action,
                    is_async: false,
                    ..
                } => dispatch_base_action(state, base_action, params)
                    .await
                    .map(ActionDispatchOutcome::Sync),
                RegisteredActionKind::Supported {
                    base_action,
                    is_async: true,
                    rate_limited,
                    ..
                } => {
                    // 先通过信号量执行非阻塞准入控制，防止恶意洪泛无限 spawn 后台协程耗尽内存
                    let Ok(async_permit) = state.async_action_semaphore.clone().try_acquire_owned()
                    else {
                        return Err(common_types::AdapterError::InputRejected(
                            "too many in-flight async OneBot actions; retry later".to_string(),
                        ));
                    };
                    let bg_state = state.clone();
                    let interval_ms = state.config.onebot.rate_limit_interval_ms;
                    tokio::spawn(async move {
                        let _permit = async_permit;
                        if rate_limited && interval_ms > 0 {
                            let min_gap = std::time::Duration::from_millis(interval_ms);
                            // 仅在微秒级临界区内预留下一执行时间戳槽位，随后立即放锁等待，避免持锁睡眠阻塞排队
                            let wait_until = {
                                let mut slot = bg_state.rate_limit_gate.lock().await;
                                let now = tokio::time::Instant::now();
                                let scheduled = match *slot {
                                    Some(prev) if prev + min_gap > now => prev + min_gap,
                                    _ => now,
                                };
                                *slot = Some(scheduled);
                                (scheduled > now).then_some(scheduled)
                            };
                            if let Some(deadline) = wait_until {
                                tokio::select! {
                                    _ = bg_state.wait_shutdown() => return,
                                    _ = tokio::time::sleep_until(deadline) => {}
                                }
                            }
                        }
                        if bg_state.is_shutting_down() {
                            return;
                        }
                        if let Err(err) = dispatch_base_action(&bg_state, base_action, params).await
                        {
                            tracing::warn!(
                                action = base_action,
                                error = %err,
                                "后台异步或限速 OneBot 动作执行失败"
                            );
                            let detail = serde_json::json!({
                                "action": base_action,
                                "rate_limited": rate_limited,
                                "error": err.to_string(),
                            })
                            .to_string();
                            let _ = bg_state
                                .driver
                                .record_audit_event("async_action_failed", &detail)
                                .await;
                        }
                    });
                    Ok(ActionDispatchOutcome::AsyncAccepted)
                }
                RegisteredActionKind::Unsupported(reason) => {
                    Err(common_types::AdapterError::UnsupportedAction {
                        action: action.to_string(),
                        reason: reason.to_string(),
                    })
                }
                RegisteredActionKind::Unknown => {
                    Err(common_types::AdapterError::ActionNotFound {
                        action: action.to_string(),
                    })
                }
            }
        }

        /// 统一执行 HTTP、正向 WS 与反向 WS 传入的 OneBot v11 基础动作请求
        pub async fn dispatch_onebot_action(
            state: &crate::AppState,
            action: &str,
            params: serde_json::Value,
        ) -> std::result::Result<serde_json::Value, common_types::AdapterError> {
            match dispatch_onebot_action_with_mode(state, action, params).await? {
                ActionDispatchOutcome::Sync(data) => Ok(data),
                ActionDispatchOutcome::AsyncAccepted => Ok(serde_json::Value::Null),
            }
        }

        /// 为 WebSocket 会话统一封装带 `status`、`retcode`、`data` 与可选 `echo` 的标准响应 JSON
        pub async fn execute_onebot_ws_envelope(
            state: &crate::AppState,
            action: &str,
            params: serde_json::Value,
            echo: Option<serde_json::Value>,
        ) -> serde_json::Value {
            match dispatch_onebot_action_with_mode(state, action, params).await {
                Ok(ActionDispatchOutcome::Sync(data)) => serde_json::json!({
                    "status": crate::constants::ONEBOT_STATUS_OK,
                    "retcode": 0,
                    "data": data,
                    "echo": echo,
                }),
                Ok(ActionDispatchOutcome::AsyncAccepted) => serde_json::json!({
                    "status": crate::constants::ONEBOT_STATUS_ASYNC,
                    "retcode": crate::constants::ONEBOT_RETCODE_ASYNC,
                    "data": serde_json::Value::Null,
                    "echo": echo,
                }),
                Err(err) => {
                    let (_, error_code, retcode) = crate::error::map_adapter_error(&err);
                    serde_json::json!({
                        "status": crate::constants::ONEBOT_STATUS_FAILED,
                        "retcode": retcode,
                        "error": err.to_string(),
                        "code": error_code,
                        "data": serde_json::Value::Null,
                        "echo": echo,
                    })
                }
            }
        }

        async fn http_onebot_action_dispatch(
            axum::extract::State(state): axum::extract::State<crate::AppState>,
            axum::extract::Path(action): axum::extract::Path<String>,
            method: axum::http::Method,
            axum::extract::Query(query): axum::extract::Query<
                std::collections::HashMap<String, String>,
            >,
            body: axum::body::Bytes,
        ) -> axum::response::Response {
            use axum::response::IntoResponse as _;

            match lookup_registered_action(&action) {
                RegisteredActionKind::Supported { post_only, .. } => {
                    if post_only && method == axum::http::Method::GET {
                        return (
                            axum::http::StatusCode::METHOD_NOT_ALLOWED,
                            axum::Json(serde_json::json!({
                                "ok": false,
                                "status": crate::constants::ONEBOT_STATUS_FAILED,
                                "retcode": 1405,
                                "code": "METHOD_NOT_ALLOWED",
                                "error": format!("OneBot action '{action}' requires POST"),
                            })),
                        )
                            .into_response();
                    }
                }
                RegisteredActionKind::Unsupported(reason) => {
                    return crate::error::ApiError(
                        common_types::AdapterError::UnsupportedAction {
                            action,
                            reason: reason.to_string(),
                        },
                    )
                    .into_response();
                }
                RegisteredActionKind::Unknown => {
                    return crate::error::ApiError(
                        common_types::AdapterError::ActionNotFound { action },
                    )
                    .into_response();
                }
            }

            let params = match $crate::onebot::payloads::parse_onebot_http_params(query, body) {
                Ok(parsed) => parsed,
                Err(err) => return crate::error::ApiError(err).into_response(),
            };

            match dispatch_onebot_action_with_mode(&state, &action, params).await {
                Ok(ActionDispatchOutcome::Sync(data)) => {
                    $crate::onebot::payloads::ok_envelope(data).into_response()
                }
                Ok(ActionDispatchOutcome::AsyncAccepted) => axum::Json(serde_json::json!({
                    "status": crate::constants::ONEBOT_STATUS_ASYNC,
                    "retcode": crate::constants::ONEBOT_RETCODE_ASYNC,
                    "data": serde_json::Value::Null,
                }))
                .into_response(),
                Err(err) => crate::error::ApiError(err).into_response(),
            }
        }

        /// 将声明表中全部 OneBot v11 HTTP 动作通过 `/onebot/v11/{action}` 与根级 `/{action}` 挂载至路由器
        pub fn onebot_routes() -> axum::Router<crate::AppState> {
            axum::Router::new()
                .route(
                    crate::constants::ROUTE_ONEBOT_V11_ACTION,
                    axum::routing::get(http_onebot_action_dispatch)
                        .post(http_onebot_action_dispatch),
                )
                .route(
                    crate::constants::ROUTE_ONEBOT_ROOT_ACTION,
                    axum::routing::get(http_onebot_action_dispatch)
                        .post(http_onebot_action_dispatch),
                )
        }
    };
    (@is_post_only get_post) => {
        false
    };
    (@is_post_only post_only) => {
        true
    };
}
