//! 公共类型与共享常量单元测试模块

use std::str::FromStr;

use super::*;
use crate::constants::{
    DEFAULT_DB_PATH, DEFAULT_ONEBOT_HEARTBEAT_MS, DEFAULT_ONEBOT_ID_BOUND,
    DEFAULT_ONEBOT_RECONNECT_MS, DEFAULT_QQ_EXECUTABLE_PATH, DEFAULT_QQ_LAUNCH_ARGS,
    DEFAULT_QQ_STARTUP_TIMEOUT_MS, DEFAULT_STRICT_BELOW_MEMBERS, DEFAULT_TARGET_GROUP_NAME,
};

#[test]
fn defaults_include_qq_client_and_hoyo_guard() {
    // 校验顶层默认配置完整包含安全人数守卫、QQ 客户端参数、存储路径与 OneBot 配置
    let adapter_config = AdapterConfig::default();
    assert_eq!(adapter_config.target.group_name, DEFAULT_TARGET_GROUP_NAME);
    assert_eq!(
        adapter_config.target.strict_below_members,
        Some(DEFAULT_STRICT_BELOW_MEMBERS)
    );
    assert_eq!(
        adapter_config.qq_client.executable_path.as_deref(),
        Some(DEFAULT_QQ_EXECUTABLE_PATH)
    );
    assert!(adapter_config.qq_client.auto_launch_or_wake);
    assert_eq!(
        adapter_config.qq_client.launch_args,
        DEFAULT_QQ_LAUNCH_ARGS
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        adapter_config.qq_client.startup_timeout_ms,
        DEFAULT_QQ_STARTUP_TIMEOUT_MS
    );
    assert_eq!(adapter_config.storage.db_path, DEFAULT_DB_PATH);
    assert!(adapter_config.onebot.reverse_ws_urls.is_empty());
    assert!(adapter_config.onebot.access_token.is_none());
    assert_eq!(
        adapter_config.onebot.heartbeat_interval_ms,
        DEFAULT_ONEBOT_HEARTBEAT_MS
    );
    assert_eq!(
        adapter_config.onebot.reconnect_interval_ms,
        DEFAULT_ONEBOT_RECONNECT_MS
    );

    let adapter_status = AdapterStatus::default();
    assert!(adapter_status.resolved_qq_path.is_none());

    let diagnostic_report = WindowDiagnosticReport::default();
    assert_eq!(diagnostic_report.target_hwnd, 0);
    assert!(diagnostic_report.all_qq_windows.is_empty());
}

#[test]
fn stable_onebot_id_is_deterministic_and_bounded() {
    let first_hash = stable_onebot_id(DEFAULT_TARGET_GROUP_NAME, DEFAULT_ONEBOT_ID_BOUND);
    let second_hash = stable_onebot_id(DEFAULT_TARGET_GROUP_NAME, DEFAULT_ONEBOT_ID_BOUND);
    let other_hash = stable_onebot_id("OtherGroup", DEFAULT_ONEBOT_ID_BOUND);
    let clamped_empty = stable_onebot_id("", 5);

    assert_eq!(first_hash, second_hash);
    assert!((1..DEFAULT_ONEBOT_ID_BOUND).contains(&first_hash));
    assert!((1..DEFAULT_ONEBOT_ID_BOUND).contains(&other_hash));
    assert_ne!(first_hash, other_hash);
    assert!((1..1000).contains(&clamped_empty));
}

#[test]
fn v1_toml_without_storage_or_onebot_parses_with_defaults() {
    let legacy_toml = r#"
[server]
host = "127.0.0.1"
port = 18770

[target]
group_name = "Demo测试群"
strict_below_members = 10
require_member_count_verified = false

[bot]
self_id = 10001
"#;
    let parsed_config: AdapterConfig = toml::from_str(legacy_toml).expect("v1 TOML should parse");
    assert_eq!(parsed_config.storage.db_path, DEFAULT_DB_PATH);
    assert!(parsed_config.onebot.reverse_ws_urls.is_empty());
    assert_eq!(
        parsed_config.onebot.heartbeat_interval_ms,
        DEFAULT_ONEBOT_HEARTBEAT_MS
    );
    assert_eq!(
        parsed_config.onebot.reconnect_interval_ms,
        DEFAULT_ONEBOT_RECONNECT_MS
    );
}

#[test]
fn message_and_outcome_defaults_and_serde_backwards_compat() {
    let chat_message = ChatMessage::default();
    assert!(chat_message.onebot_message_id.is_none());
    assert!(chat_message.group_id.is_none());
    assert!(chat_message.user_id.is_none());

    let send_outcome = SendOutcome::default();
    assert_eq!(send_outcome.used_mode, InputMode::Auto);
    assert!(send_outcome.onebot_message_id.is_none());
    assert!(send_outcome.group_id.is_none());
}

#[test]
fn input_mode_and_shortcut_strum_aliases_and_display() {
    // 校验所有历史 CLI 与配置文件别名均可解析且展示值保持 snake_case
    assert_eq!(
        InputMode::from_str("background_uia").expect("parse background_uia"),
        InputMode::BackgroundUia
    );
    assert_eq!(
        InputMode::from_str("background-uia").expect("parse background-uia"),
        InputMode::BackgroundUia
    );
    assert_eq!(
        InputMode::from_str("background").expect("parse background"),
        InputMode::BackgroundUia
    );
    assert_eq!(
        InputMode::from_str("focus_restore_unicode").expect("parse focus_restore_unicode"),
        InputMode::FocusRestoreUnicode
    );
    assert_eq!(
        InputMode::from_str("focus-restore-unicode").expect("parse focus-restore-unicode"),
        InputMode::FocusRestoreUnicode
    );
    assert_eq!(
        InputMode::from_str("FOCUS").expect("parse FOCUS"),
        InputMode::FocusRestoreUnicode
    );
    assert_eq!(
        InputMode::from_str("Auto").expect("parse Auto"),
        InputMode::Auto
    );
    assert_eq!(InputMode::BackgroundUia.to_string(), "background_uia");
    assert_eq!(
        InputMode::FocusRestoreUnicode.to_string(),
        "focus_restore_unicode"
    );
    assert_eq!(InputMode::Auto.to_string(), "auto");

    assert_eq!(
        SendShortcut::from_str("enter").expect("parse enter"),
        SendShortcut::Enter
    );
    assert_eq!(
        SendShortcut::from_str("ctrl_enter").expect("parse ctrl_enter"),
        SendShortcut::CtrlEnter
    );
    assert_eq!(
        SendShortcut::from_str("ctrl-enter").expect("parse ctrl-enter"),
        SendShortcut::CtrlEnter
    );
    assert_eq!(SendShortcut::CtrlEnter.to_string(), "ctrl_enter");
}

#[test]
fn infer_qq_max_member_count_and_canonical_group_matching() {
    assert_eq!(infer_qq_max_member_count(Some(9), None), 200);
    assert_eq!(infer_qq_max_member_count(Some(250), None), 500);
    assert_eq!(infer_qq_max_member_count(Some(800), None), 1000);
    assert_eq!(infer_qq_max_member_count(Some(1500), None), 2000);
    assert_eq!(infer_qq_max_member_count(Some(2500), None), 3000);
    assert_eq!(infer_qq_max_member_count(Some(9), Some(500)), 500);
    assert_eq!(infer_qq_max_member_count(None, None), 0);

    assert_eq!(strip_group_count_suffix("Demo测试群 (9)"), "Demo测试群");
    assert_eq!(strip_group_count_suffix("Demo测试群（26人）"), "Demo测试群");
    assert_eq!(strip_group_count_suffix("测试群(Pro)"), "测试群(Pro)");
    assert!(canonical_group_names_match("Demo测试群 (9)", "Demo测试群"));
    assert!(!canonical_group_names_match(
        "Demo测试群2 (5)",
        "Demo测试群"
    ));
}

#[test]
fn message_segment_cq_codec_and_json_roundtrip() {
    let raw = "[CQ:reply,id=10005][CQ:at,qq=200000001,name=测试机器人] 你好 &#91;测试&#93;[CQ:image,file=base64://AAAA]";
    let segments = parse_cq_string_to_segments(raw, false);
    assert_eq!(segments.len(), 4);
    assert_eq!(
        segments[0],
        MessageSegment::Reply {
            id: "10005".to_string()
        }
    );
    assert_eq!(
        segments[1],
        MessageSegment::At {
            qq: "200000001".to_string(),
            name: Some("测试机器人".to_string())
        }
    );
    assert_eq!(
        segments[2],
        MessageSegment::Text {
            text: " 你好 [测试]".to_string()
        }
    );
    assert_eq!(
        segments[3],
        MessageSegment::Image {
            file: "base64://AAAA".to_string(),
            url: None,
            summary: None,
            image_type: None,
        }
    );

    let re_encoded = segments_to_cq_string(&segments);
    assert_eq!(re_encoded, raw);

    let json_payload = serde_json::json!([
        { "type": "reply", "data": { "id": 10005 } },
        { "type": "at", "data": { "qq": 200000001 } },
        { "type": "text", "data": { "text": " 你好" } }
    ]);
    let parsed_json =
        parse_onebot_message_value(&json_payload, false).expect("parse array segments");
    assert_eq!(parsed_json.len(), 3);
    assert_eq!(
        parsed_json[0],
        MessageSegment::Reply {
            id: "10005".to_string()
        }
    );
    assert_eq!(
        parsed_json[1],
        MessageSegment::At {
            qq: "200000001".to_string(),
            name: None
        }
    );
}

#[test]
fn all_twenty_one_segments_and_face_table_roundtrip() {
    assert_eq!(face_id_to_name("14"), Some("微笑"));
    assert_eq!(face_id_to_name("179"), Some("doge"));
    assert_eq!(face_name_to_id("微笑"), Some("14"));
    assert_eq!(face_name_to_id("[DOGE]"), Some("179"));
    assert_eq!(face_name_to_id("/比心"), Some("319"));

    let all_segments = vec![
        MessageSegment::Text {
            text: "文本".to_string(),
        },
        MessageSegment::Face {
            id: "14".to_string(),
            text: None,
        },
        MessageSegment::Image {
            file: "a.png".to_string(),
            url: Some("http://img.local/a.png".to_string()),
            summary: None,
            image_type: Some("flash".to_string()),
        },
        MessageSegment::Record {
            file: "v.amr".to_string(),
            url: None,
            magic: Some("1".to_string()),
        },
        MessageSegment::Video {
            file: "clip.mp4".to_string(),
            url: None,
        },
        MessageSegment::At {
            qq: "all".to_string(),
            name: None,
        },
        MessageSegment::Rps {},
        MessageSegment::Dice {},
        MessageSegment::Shake {},
        MessageSegment::Poke {
            poke_type: "poke".to_string(),
            id: "126".to_string(),
            name: Some("戳一戳".to_string()),
        },
        MessageSegment::Anonymous {
            ignore: Some("1".to_string()),
        },
        MessageSegment::Share {
            url: "http://example.com".to_string(),
            title: "示例".to_string(),
            content: Some("摘要".to_string()),
            image: None,
        },
        MessageSegment::Contact {
            contact_type: "group".to_string(),
            id: "100000001".to_string(),
        },
        MessageSegment::Location {
            lat: "31.23".to_string(),
            lon: "121.47".to_string(),
            title: Some("上海".to_string()),
            content: None,
        },
        MessageSegment::Music {
            music_type: "qq".to_string(),
            id: Some("1024".to_string()),
            url: None,
            audio: None,
            title: None,
            content: None,
            image: None,
        },
        MessageSegment::Reply {
            id: "999".to_string(),
        },
        MessageSegment::Forward {
            id: "fwd-1".to_string(),
        },
        MessageSegment::Node {
            id: None,
            user_id: Some("10001".to_string()),
            nickname: Some("Bot".to_string()),
            content: Some(serde_json::json!("节点文本")),
        },
        MessageSegment::Xml {
            data: "<msg/>".to_string(),
        },
        MessageSegment::Json {
            data: "{\"app\":\"card\"}".to_string(),
        },
        MessageSegment::File {
            file: "report.pdf".to_string(),
            name: Some("报告".to_string()),
            size: None,
        },
    ];
    assert_eq!(all_segments.len(), 21);

    let rps_json = serde_json::to_value(&MessageSegment::Rps {}).unwrap();
    assert_eq!(rps_json, serde_json::json!({ "type": "rps", "data": {} }));

    let serialized_array = serde_json::to_value(&all_segments).unwrap();
    let parsed_from_json =
        parse_onebot_message_value(&serialized_array, false).expect("parse 21 json segments");
    assert_eq!(parsed_from_json.len(), 21);
    assert_eq!(parsed_from_json[6], MessageSegment::Rps {});
    assert_eq!(parsed_from_json[7], MessageSegment::Dice {});
    assert_eq!(parsed_from_json[8], MessageSegment::Shake {});

    let cq = segments_to_cq_string(&all_segments);
    assert!(cq.contains("[CQ:face,id=14]"));
    assert!(cq.contains("[CQ:rps]"));
    assert!(cq.contains("[CQ:dice]"));
    assert!(cq.contains("[CQ:shake]"));
    assert!(cq.contains("[CQ:forward,id=fwd-1]"));
    assert!(cq.contains("[CQ:file,file=report.pdf,name=报告]"));

    let summary = segments_to_plain_summary(&all_segments);
    assert!(summary.contains("[表情:微笑]"));
    assert!(summary.contains("[闪照]"));
    assert!(summary.contains("[猜拳]"));
    assert!(summary.contains("[文件:报告]"));
}

#[test]
fn private_friend_whitelist_rejects_empty_and_matches_exact() {
    let empty_spec = GroupTargetSpec::default();
    assert!(!empty_spec.is_private_friend_allowed("测试好友A"));

    let allow_spec = GroupTargetSpec {
        allowed_private_friends: vec!["测试好友A".to_string(), "测试好友".to_string()],
        ..Default::default()
    };
    assert!(allow_spec.is_private_friend_allowed("测试好友A"));
    assert!(allow_spec.is_private_friend_allowed("测试好友"));
    assert!(!allow_spec.is_private_friend_allowed("陌生人甲"));
}

#[test]
fn cq_codec_preserves_param_whitespace_and_handles_comma_bomb_and_i64_bounds() {
    // 验证 QA-BUG-007：CQ 参数值首尾空格与换行符往返无损
    let segs = vec![
        MessageSegment::File {
            file: "doc.txt".to_string(),
            name: Some("  带空格文件名 \n第二行 ".to_string()),
            size: None,
        },
        MessageSegment::Share {
            url: "http://example.com".to_string(),
            title: " 前导空格标题 ".to_string(),
            content: Some("  正文\n换行  ".to_string()),
            image: None,
        },
    ];
    let cq = segments_to_cq_string(&segs);
    let parsed = parse_cq_string_to_segments(&cq, false);
    assert_eq!(parsed, segs);

    // 验证未闭合超长逗号炸弹不会引起正则回溯退化并能在毫秒内完成解析
    let mut bomb = String::from("[CQ:face");
    for idx in 0..4096 {
        bomb.push_str(",k=");
        bomb.push_str(&idx.to_string());
    }
    let started = std::time::Instant::now();
    let parsed_bomb = parse_cq_string_to_segments(&bomb, false);
    assert!(started.elapsed() < std::time::Duration::from_millis(200));
    assert_eq!(parsed_bomb.len(), 1);

    // 验证极端整数边界输入下确定性标识映射不发生有符号溢出
    for bound in [i64::MIN, 0, i64::MAX] {
        let id = stable_onebot_id("极端边界群名称", bound);
        assert!(id >= 1);
    }
}

/// 最小化驱动契约实现桩，用于验证 `BotDriver::cached_status` 默认返回 `None`
struct MinimalDummyDriver;

#[async_trait::async_trait]
impl BotDriver for MinimalDummyDriver {
    async fn status(&self) -> Result<AdapterStatus> {
        Ok(AdapterStatus::default())
    }

    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(Vec::new())
    }

    async fn inspect_tree(
        &self,
        _hwnd: Option<isize>,
        _max_depth: usize,
    ) -> Result<UiNodeSnapshot> {
        Ok(UiNodeSnapshot::default())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        Ok(Vec::new())
    }

    async fn open_group(&self, _spec: GroupTargetSpec) -> Result<AdapterStatus> {
        Ok(AdapterStatus::default())
    }

    async fn poll_messages(&self, _group: &str, _limit: usize) -> Result<Vec<ChatMessage>> {
        Ok(Vec::new())
    }

    async fn send_group_message(&self, _req: SendRequest) -> Result<SendOutcome> {
        Ok(SendOutcome::default())
    }

    fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<AdapterEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(4);
        drop(tx);
        rx
    }
}

#[test]
fn face_prefix_stripping_and_degraded_contracts_work() {
    // 验证表情名称反查支持 `[表情:名称]` 与 `表情：名称` 前缀剥离
    assert_eq!(face_name_to_id("[表情:微笑]"), Some("14"));
    assert_eq!(face_name_to_id("表情:微笑"), Some("14"));
    assert_eq!(face_name_to_id("表情：微笑"), Some("14"));
    assert_eq!(face_name_to_id("[表情：DOGE]"), Some("179"));
    assert_eq!(face_name_to_id("/表情:微笑"), Some("14"));
    assert_eq!(face_name_to_id("[表情:]"), None);
    assert_eq!(face_name_to_id(""), None);

    // 验证 SendOutcome 默认包含空 degraded_segments 且兼容历史 JSON 反序列化
    let default_outcome = SendOutcome::default();
    assert!(default_outcome.degraded_segments.is_empty());
    let legacy_json = serde_json::json!({
        "ok": true,
        "group": "Demo测试群",
        "used_mode": "auto",
        "foreground_touched": false,
        "mouse_touched": false,
        "verified_in_chat": true,
        "elapsed_ms": 12,
        "detail": "ok"
    });
    let deserialized_legacy: SendOutcome =
        serde_json::from_value(legacy_json).expect("legacy SendOutcome JSON should parse");
    assert!(deserialized_legacy.degraded_segments.is_empty());

    let outcome_with_degraded = SendOutcome {
        degraded_segments: vec!["face".to_string(), "reply".to_string()],
        ..Default::default()
    };
    let roundtrip: SendOutcome = serde_json::from_str(
        &serde_json::to_string(&outcome_with_degraded).expect("serialize SendOutcome"),
    )
    .expect("deserialize SendOutcome");
    assert_eq!(
        roundtrip.degraded_segments,
        vec!["face".to_string(), "reply".to_string()]
    );

    // 验证 OneBotConfig.allow_degraded_media_summary 默认关闭并支持显式开启
    let default_cfg = OneBotConfig::default();
    assert!(!default_cfg.allow_degraded_media_summary);
    let legacy_toml_cfg: AdapterConfig = toml::from_str("[onebot]\npost_message_sent = true\n")
        .expect("legacy onebot TOML parses with default allow_degraded_media_summary");
    assert!(!legacy_toml_cfg.onebot.allow_degraded_media_summary);
    let opt_in_toml_cfg: AdapterConfig =
        toml::from_str("[onebot]\nallow_degraded_media_summary = true\n")
            .expect("opt-in onebot TOML parses");
    assert!(opt_in_toml_cfg.onebot.allow_degraded_media_summary);

    // 验证 BotDriver::cached_status 默认实现返回 None
    let dummy_driver = MinimalDummyDriver;
    assert!(dummy_driver.cached_status().is_none());

    // 验证标准 [CQ:poke,qq=...] / JSON poke 与未知 CQ 严格拦截检测
    let poke_cq = parse_cq_string_to_segments("[CQ:poke,qq=200000002]", false);
    assert_eq!(
        poke_cq,
        vec![MessageSegment::Poke {
            poke_type: "poke".to_string(),
            id: "200000002".to_string(),
            name: None,
        }]
    );
    let poke_json = parse_onebot_message_value(
        &serde_json::json!([{ "type": "poke", "data": { "qq": 200000002 } }]),
        false,
    )
    .expect("json poke with qq parses");
    assert_eq!(poke_json, poke_cq);

    let poke_combo_cq = parse_cq_string_to_segments("[CQ:poke,type=1,id=126,qq=200000002]", false);
    assert_eq!(
        poke_combo_cq,
        vec![MessageSegment::Poke {
            poke_type: "1".to_string(),
            id: "200000002".to_string(),
            name: None,
        }]
    );
    let poke_combo_json = parse_onebot_message_value(
        &serde_json::json!([{ "type": "poke", "data": { "type": "1", "id": 126, "qq": 200000002 } }]),
        false,
    )
    .expect("json poke with qq and id prefers qq");
    assert_eq!(poke_combo_json, poke_combo_cq);

    assert!(has_unparsed_cq_tag("[CQ:unknown,x=1]"));
    assert!(has_unparsed_cq_tag("[CQ:poke]"));
    assert!(!has_unparsed_cq_tag("[CQ:poke,qq=200000002]"));
    assert!(!has_unparsed_cq_tag("&#91;CQ:face,id=14&#93;"));
    assert!(!has_unparsed_onebot_value(
        &serde_json::json!("[CQ:unknown]"),
        true
    ));
    assert!(has_unparsed_onebot_value(
        &serde_json::json!([{ "type": "unknown_seg", "data": {} }]),
        false
    ));
}

#[test]
fn rest_request_dtos_and_routes_deserialize_aliases() {
    assert_eq!(constants::ROUTE_API_OPEN_PRIVATE, "/api/open-private");
    assert_eq!(constants::ROUTE_API_DELETE_MSG, "/api/delete-msg");
    assert_eq!(
        constants::ROUTE_API_MESSAGES_HISTORY,
        "/api/messages/history"
    );

    let from_target: OpenPrivateRequest = serde_json::from_str(r#"{"target":"测试好友"}"#).unwrap();
    let from_nickname: OpenPrivateRequest =
        serde_json::from_str(r#"{"nickname":"测试好友"}"#).unwrap();
    let from_friend: OpenPrivateRequest = serde_json::from_str(r#"{"friend":"测试好友"}"#).unwrap();
    let from_group: OpenPrivateRequest = serde_json::from_str(r#"{"group":"测试好友"}"#).unwrap();
    assert_eq!(from_target.target, "测试好友");
    assert_eq!(from_nickname, from_target);
    assert_eq!(from_friend, from_target);
    assert_eq!(from_group, from_target);

    let del: DeleteMessageRequest = serde_json::from_str(r#"{"message_id":12345}"#).unwrap();
    assert_eq!(del.message_id, 12345);
}

#[test]
fn scheduler_config_defaults_and_default_toml_match() {
    // 校验洪峰调度器默认参数及与 config/default.toml 的一致性
    let defaults = SchedulerConfig::default();
    assert!(defaults.enabled);
    assert_eq!(defaults.burst_window_ms, 3000);
    assert_eq!(defaults.burst_enter_threshold, 16);
    assert_eq!(defaults.burst_exit_threshold, 6);
    assert_eq!(defaults.single_poll_burst_threshold, 12);
    assert_eq!(defaults.critical_response_window_ms, 30_000);
    assert_eq!(defaults.contextual_response_window_ms, 20_000);
    assert_eq!(defaults.ordinary_response_window_ms, 8_000);
    assert_eq!(defaults.burst_ordinary_sample_stride, 5);
    assert_eq!(defaults.hard_shed_queue_depth, 64);
    assert_eq!(defaults.max_pending_events, 1024);
    assert!(defaults.drop_expired_events);
    assert_eq!(defaults.session_dwell_min_ms, 1200);
    assert_eq!(defaults.urgent_preempt_dwell_ms, 350);
    assert_eq!(defaults.patrol_interval_ms, 2500);
    assert_eq!(defaults.group_quantum_messages, 8);
    assert_eq!(defaults.active_thread_ttl_ms, 20_000);
    assert_eq!(defaults.command_prefixes, vec!["/", "!", "！", ".", "。"]);
    assert!(defaults.wake_keywords.is_empty());

    let partial: AdapterConfig =
        toml::from_str("[scheduler]\nenabled = false\nburst_enter_threshold = 24\n")
            .expect("partial [scheduler] TOML should parse");
    assert!(!partial.scheduler.enabled);
    assert_eq!(partial.scheduler.burst_enter_threshold, 24);
    assert_eq!(
        partial.scheduler.ordinary_response_window_ms,
        defaults.ordinary_response_window_ms
    );

    let default_toml_raw = include_str!("../../../config/default.toml");
    let default_toml_cfg: AdapterConfig =
        toml::from_str(default_toml_raw).expect("config/default.toml should parse");
    assert_eq!(default_toml_cfg.scheduler, defaults);
    assert_eq!(default_toml_cfg.target.group_name, "Demo测试群");
    assert_eq!(default_toml_cfg.target.strict_below_members, Some(10));
    assert!(default_toml_cfg.target.monitored_groups.is_empty());
    assert!(!default_toml_cfg.target.allow_large_monitored_groups);
}

#[test]
fn scheduler_types_priority_ordering_and_serde_roundtrip() {
    // 校验消息优先级分层序关系、默认枚举值与遥测快照往返序列化
    assert_eq!(
        MessagePriorityTier::default(),
        MessagePriorityTier::OrdinaryChatter
    );
    assert!(MessagePriorityTier::CriticalBotTrigger < MessagePriorityTier::HighContextual);
    assert!(MessagePriorityTier::HighContextual < MessagePriorityTier::OrdinaryChatter);
    assert_eq!(
        serde_json::to_string(&MessagePriorityTier::CriticalBotTrigger).unwrap(),
        "\"critical_bot_trigger\""
    );
    assert_eq!(
        serde_json::to_string(&MessagePriorityTier::HighContextual).unwrap(),
        "\"high_contextual\""
    );
    assert_eq!(
        serde_json::to_string(&MessagePriorityTier::OrdinaryChatter).unwrap(),
        "\"ordinary_chatter\""
    );

    assert_eq!(BurstModeState::default(), BurstModeState::Normal);
    assert_eq!(
        serde_json::to_string(&BurstModeState::BurstActive).unwrap(),
        "\"burst_active\""
    );
    assert_eq!(
        serde_json::from_str::<BurstModeState>("\"normal\"").unwrap(),
        BurstModeState::Normal
    );

    let snapshot = SchedulerTelemetrySnapshot {
        burst_state: BurstModeState::BurstActive,
        burst_enter_count: 2,
        recent_window_arrivals: 24,
        pending_total: 10,
        pending_p0: 2,
        pending_p1: 3,
        pending_p2: 5,
        received_total: 100,
        dispatched_p0_total: 12,
        dispatched_p1_total: 20,
        dispatched_p2_total: 40,
        selective_shed_p2_total: 15,
        expired_window_total: 3,
        outbound_deadline_aborted_total: 1,
        multi_group_switches_total: 4,
        last_dispatched_group: Some("大群Alpha".to_string()),
        active_monitored_groups: 2,
    };
    let encoded = serde_json::to_string(&snapshot).unwrap();
    let decoded: SchedulerTelemetrySnapshot = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, snapshot);
    let partial: SchedulerTelemetrySnapshot = serde_json::from_str("{}").unwrap();
    assert_eq!(partial, SchedulerTelemetrySnapshot::default());
}

#[test]
fn group_target_spec_monitored_groups_and_member_guard() {
    // 校验默认单群回退、多群去重匹配以及严格人数上限安全守卫仅对显式放行监控大群解除
    let default_spec = GroupTargetSpec::default();
    assert!(default_spec.monitored_groups.is_empty());
    assert!(!default_spec.allow_large_monitored_groups);
    assert_eq!(
        default_spec.effective_monitored_groups(),
        vec!["Demo测试群".to_string()]
    );
    assert!(default_spec.is_monitored_group("Demo测试群 (9)"));
    assert!(!default_spec.is_monitored_group("大群Alpha (520)"));
    assert_eq!(
        default_spec
            .member_guard_for_group("Demo测试群")
            .strict_below_members,
        Some(10)
    );

    let locked_multi = GroupTargetSpec {
        monitored_groups: vec!["大群Alpha".to_string(), "大群Beta".to_string()],
        allow_large_monitored_groups: false,
        ..Default::default()
    };
    assert_eq!(
        locked_multi
            .member_guard_for_group("大群Alpha (520)")
            .strict_below_members,
        Some(10)
    );

    let allowed_multi = GroupTargetSpec {
        monitored_groups: vec![
            " 大群Alpha ".to_string(),
            "大群Beta".to_string(),
            "大群Alpha (520)".to_string(),
            "".to_string(),
        ],
        allow_large_monitored_groups: true,
        ..Default::default()
    };
    assert_eq!(
        allowed_multi.effective_monitored_groups(),
        vec!["大群Alpha".to_string(), "大群Beta".to_string()]
    );
    assert!(allowed_multi.is_monitored_group("大群Alpha (520)"));
    assert!(allowed_multi.is_monitored_group("大群Beta（680人）"));
    assert!(!allowed_multi.is_monitored_group("Demo测试群"));
    assert!(!allowed_multi.is_monitored_group("未监控群 (500)"));
    assert!(!allowed_multi.is_monitored_group("   "));

    let alpha_guard = allowed_multi.member_guard_for_group("大群Alpha (520)");
    assert_eq!(alpha_guard.group_name, "大群Alpha (520)");
    assert_eq!(alpha_guard.strict_below_members, None);

    let outside_guard = allowed_multi.member_guard_for_group("未监控大群 (800)");
    assert_eq!(outside_guard.group_name, "未监控大群 (800)");
    assert_eq!(outside_guard.strict_below_members, Some(10));

    let blank_guard = allowed_multi.member_guard_for_group("   ");
    assert_eq!(blank_guard.strict_below_members, Some(10));

    let empty_spec = GroupTargetSpec {
        group_name: " ".to_string(),
        monitored_groups: Vec::new(),
        ..Default::default()
    };
    assert!(empty_spec.effective_monitored_groups().is_empty());
    assert!(!empty_spec.is_monitored_group("大群Alpha"));
}

#[test]
fn extended_messaging_error_and_constants_contracts() {
    // 校验扩展消息字段后向兼容、响应窗超时错误格式与调度器路由动作常量
    let legacy_session: SessionSummary = serde_json::from_value(serde_json::json!({
        "title": "大群Alpha (520)",
        "raw_name": "大群Alpha (520)",
        "preview": null,
        "unread_count": 3,
        "member_count_hint": 520,
        "rect": [0, 0, 200, 40]
    }))
    .unwrap();
    assert!(!legacy_session.has_at_me_hint);
    assert!(!legacy_session.has_at_all_hint);

    let chat_msg = ChatMessage {
        sender_role: Some("admin".to_string()),
        sender_card: Some("管理员甲".to_string()),
        priority_tier: Some(MessagePriorityTier::CriticalBotTrigger),
        response_deadline_ms: Some(1_700_000_030_000),
        reply_targets_self: true,
        ..Default::default()
    };
    let chat_json = serde_json::to_value(&chat_msg).unwrap();
    assert_eq!(chat_json["priority_tier"], "critical_bot_trigger");
    assert_eq!(chat_json["reply_targets_self"], true);
    let default_chat_json = serde_json::to_value(ChatMessage::default()).unwrap();
    assert!(default_chat_json.get("sender_role").is_none());
    assert!(default_chat_json.get("priority_tier").is_none());

    let send_req = SendRequest {
        deadline_unix_ms: Some(1_700_000_030_000),
        ..Default::default()
    };
    let send_json = serde_json::to_value(&send_req).unwrap();
    assert_eq!(send_json["deadline_unix_ms"], 1_700_000_030_000u64);
    assert!(serde_json::to_value(SendRequest::default())
        .unwrap()
        .get("deadline_unix_ms")
        .is_none());

    let status = AdapterStatus::default();
    assert!(!status.burst_mode_active);
    assert_eq!(status.scheduler_pending, 0);

    let err = AdapterError::ResponseWindowExpired {
        target: "大群Alpha".to_string(),
        elapsed_ms: 9200,
        window_ms: 8000,
    };
    let err_text = err.to_string();
    assert!(err_text.contains("大群Alpha"));
    assert!(err_text.contains("9200"));
    assert!(err_text.contains("8000"));

    assert_eq!(constants::ROUTE_API_SCHEDULER, "/api/scheduler");
    assert_eq!(
        constants::ROUTE_API_SCHEDULER_POLICY,
        "/api/scheduler/policy"
    );
    assert_eq!(constants::ROUTE_API_SESSIONS_READ, "/api/sessions/read");
    assert_eq!(constants::ACTION_MARK_MSG_AS_READ, "mark_msg_as_read");
    assert_eq!(
        constants::ACTION_MARK_GROUP_MSG_AS_READ,
        "mark_group_msg_as_read"
    );
    assert_eq!(
        constants::ACTION_MARK_PRIVATE_MSG_AS_READ,
        "mark_private_msg_as_read"
    );
    assert_eq!(constants::ACTION_GET_WORD_SLICES, ".get_word_slices");
    assert_eq!(
        constants::ACTION_GET_SCHEDULER_STATUS,
        ".get_scheduler_status"
    );
}

#[tokio::test]
async fn bot_driver_scheduler_defaults_work() {
    // 校验 BotDriver 新增调度遥测、策略读写、已读标记与过期中止默认实现
    let dummy = MinimalDummyDriver;
    assert_eq!(
        dummy.scheduler_telemetry().await.unwrap(),
        SchedulerTelemetrySnapshot::default()
    );
    assert_eq!(
        dummy.scheduler_config().await.unwrap(),
        SchedulerConfig::default()
    );
    assert!(matches!(
        dummy
            .update_scheduler_config(SchedulerConfig::default())
            .await,
        Err(AdapterError::UnsupportedAction { ref action, .. })
            if action == "update_scheduler_config"
    ));
    dummy
        .mark_session_read("大群Alpha", ChatTargetKind::Group)
        .await
        .unwrap();
    dummy.record_outbound_deadline_abort();
    assert!(matches!(
        dummy.ocr_image("demo.png", None).await,
        Err(AdapterError::UnsupportedAction { ref action, .. }) if action == "ocr_image"
    ));
}

#[test]
fn ocr_dtos_file_size_compat_and_ocr_constants_work() {
    // 校验 File 消息段可选 size 字段在 CQ、JSON 与 serde 下的前后向兼容性
    let legacy_file: MessageSegment =
        serde_json::from_str(r#"{"type":"file","data":{"file":"a.pdf","name":"文档"}}"#)
            .expect("legacy file json without size");
    assert_eq!(
        legacy_file,
        MessageSegment::File {
            file: "a.pdf".to_string(),
            name: Some("文档".to_string()),
            size: None,
        }
    );
    let legacy_ser = serde_json::to_value(&legacy_file).expect("serialize legacy file");
    assert!(legacy_ser["data"].get("size").is_none());

    let sized_file = MessageSegment::File {
        file: "b.zip".to_string(),
        name: Some("归档".to_string()),
        size: Some(4096),
    };
    let cq = segments_to_cq_string(std::slice::from_ref(&sized_file));
    assert_eq!(cq, "[CQ:file,file=b.zip,name=归档,size=4096]");
    assert_eq!(
        parse_cq_string_to_segments(&cq, false),
        vec![sized_file.clone()]
    );

    let parsed_json = parse_onebot_message_value(
        &serde_json::json!([{ "type": "file", "data": { "file": "b.zip", "name": "归档", "size": 4096 } }]),
        false,
    )
    .expect("parse json file with size");
    assert_eq!(parsed_json, vec![sized_file]);

    // 校验 OCR DTO 与新增路由动作常量
    let req: OcrImageRequest = serde_json::from_str(r#"{"image":"base64://AAA"}"#).unwrap();
    assert_eq!(req.image, "base64://AAA");
    assert!(req.language.is_none());

    let resp = OcrImageResponse {
        texts: vec![OcrTextDetection {
            text: "测试文本".to_string(),
            confidence: 100,
            coordinates: vec![
                OcrPoint { x: 1, y: 2 },
                OcrPoint { x: 20, y: 2 },
                OcrPoint { x: 20, y: 18 },
                OcrPoint { x: 1, y: 18 },
            ],
        }],
        language: "zh-Hans-CN".to_string(),
    };
    let resp_roundtrip: OcrImageResponse =
        serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
    assert_eq!(resp_roundtrip, resp);

    assert_eq!(constants::ROUTE_API_OCR, "/api/ocr");
    assert_eq!(constants::ACTION_OCR_IMAGE, "ocr_image");
    assert_eq!(constants::ACTION_DOT_OCR_IMAGE, ".ocr_image");
    assert_eq!(constants::ACTION_CAN_SEND_VIDEO, "can_send_video");
    assert_eq!(constants::ACTION_UPLOAD_GROUP_FILE, "upload_group_file");
    assert_eq!(constants::ACTION_UPLOAD_PRIVATE_FILE, "upload_private_file");
    assert_eq!(constants::ACTION_GET_FILE, "get_file");
}

/// 携带内存消息夹具的驱动测试桩，用于验证 `BotDriver::query_messages_history` 默认过滤逻辑
struct HistoryDummyDriver(Vec<StoredMessageRecord>);

#[async_trait::async_trait]
impl BotDriver for HistoryDummyDriver {
    async fn status(&self) -> Result<AdapterStatus> {
        Ok(AdapterStatus::default())
    }
    async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(Vec::new())
    }
    async fn inspect_tree(&self, _: Option<isize>, _: usize) -> Result<UiNodeSnapshot> {
        Ok(UiNodeSnapshot::default())
    }
    async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        Ok(Vec::new())
    }
    async fn open_group(&self, _: GroupTargetSpec) -> Result<AdapterStatus> {
        Ok(AdapterStatus::default())
    }
    async fn poll_messages(&self, _: &str, _: usize) -> Result<Vec<ChatMessage>> {
        Ok(Vec::new())
    }
    async fn send_group_message(&self, _: SendRequest) -> Result<SendOutcome> {
        Ok(SendOutcome::default())
    }
    fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<AdapterEvent> {
        tokio::sync::broadcast::channel(4).1
    }
    async fn get_recent_messages(
        &self,
        session: &str,
        _: usize,
    ) -> Result<Vec<StoredMessageRecord>> {
        Ok(self
            .0
            .iter()
            .filter(|r| session.is_empty() || r.group_name == session)
            .cloned()
            .collect())
    }
}

#[tokio::test]
async fn wave6_history_audit_contracts_and_driver_defaults_work() {
    for (actual, expected) in [
        (constants::ROUTE_API_FORWARD_MSG, "/api/forward-msg"),
        (constants::ROUTE_API_AUDIT_EVENTS, "/api/audit-events"),
        (constants::ROUTE_API_SECURITY_RESET, "/api/security/reset"),
        (constants::ROUTE_API_CACHE_CLEAN, "/api/cache/clean"),
        (
            constants::ACTION_FORWARD_GROUP_SINGLE_MSG,
            "forward_group_single_msg",
        ),
        (
            constants::ACTION_FORWARD_FRIEND_SINGLE_MSG,
            "forward_friend_single_msg",
        ),
        (constants::ACTION_GET_RECENT_CONTACT, "get_recent_contact"),
        (constants::ACTION_GET_AUDIT_EVENTS, ".get_audit_events"),
        (
            constants::ACTION_RESET_SECURITY_BREAKER,
            ".reset_security_breaker",
        ),
    ] {
        assert_eq!(actual, expected);
    }

    let filter: MessageHistoryFilter =
        serde_json::from_str(r#"{"session_name":"G","limit":20}"#).unwrap();
    assert_eq!((filter.limit, filter.reverse_order), (20, false));
    let audit = AuditEventRecord {
        id: 7,
        event_kind: "security_breaker".into(),
        detail_json: r#"{"tripped":true}"#.into(),
        created_at_ms: 1_700_000_000_123,
    };
    assert_eq!(
        serde_json::from_str::<AuditEventRecord>(&serde_json::to_string(&audit).unwrap()).unwrap(),
        audit
    );
    let fwd: SingleForwardRequest =
        serde_json::from_str(r#"{"message_id":10042,"target":"G","target_kind":"group"}"#).unwrap();
    assert_eq!(
        (fwd.message_id, fwd.target_kind, fwd.deadline_unix_ms),
        (10042, ChatTargetKind::Group, None)
    );

    let minimal = MinimalDummyDriver;
    assert!(minimal
        .list_audit_events(None, 50)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        minimal.reset_security_breaker().await,
        Err(AdapterError::UnsupportedAction { .. })
    ));
    assert!(matches!(
        minimal.forward_single_message(fwd).await,
        Err(AdapterError::UnsupportedAction { .. })
    ));

    let driver = HistoryDummyDriver(
        (10_001..=10_006)
            .map(|id| StoredMessageRecord {
                onebot_message_id: id,
                group_name: "G".into(),
                sender_name: if id % 2 == 0 { "Alice" } else { "Bob" }.into(),
                raw_message: format!("Hello_{id}"),
                chat_type: if id == 10_006 {
                    ChatTargetKind::Private
                } else {
                    ChatTargetKind::Group
                },
                ..Default::default()
            })
            .collect(),
    );
    let paged = driver
        .query_messages_history(MessageHistoryFilter {
            session_name: "G".into(),
            chat_type: Some(ChatTargetKind::Group),
            before_or_at_id: Some(10_004),
            after_id: Some(10_001),
            sender: Some("alice".into()),
            keyword: Some("hello_".into()),
            limit: 10,
            reverse_order: false,
        })
        .await
        .unwrap();
    assert_eq!(
        paged
            .iter()
            .map(|r| r.onebot_message_id)
            .collect::<Vec<_>>(),
        vec![10_002, 10_004]
    );

    let after = driver
        .query_messages_history(MessageHistoryFilter {
            session_name: "G".into(),
            chat_type: Some(ChatTargetKind::Group),
            after_id: Some(10_001),
            limit: 2,
            reverse_order: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        after
            .iter()
            .map(|r| r.onebot_message_id)
            .collect::<Vec<_>>(),
        vec![10_003, 10_002]
    );
}
