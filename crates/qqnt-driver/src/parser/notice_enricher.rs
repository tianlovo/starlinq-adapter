//! 被动系统通知（群/私聊撤回与消息回应）的存储关联与真实 `message_id` 回填增强器

use std::collections::HashSet;

use serde_json::{json, Value};
use storage::Storage;

use super::notices::{NoticeContext, ParsedSystemNotice};
use super::parse_notice_text_to_onebot_json;
use super::row_cleaner::canonical_group_name;

/// 将 UIA 原始系统通知列表转换为 OneBot notice 事件并回填撤回与表情回应的真实消息编号
pub fn build_and_enrich_notice_events(
    storage: &Storage,
    ctx: &NoticeContext<'_>,
    fresh_notices: &[ParsedSystemNotice],
    disappeared_uids: &[String],
    time_secs: i64,
) -> Vec<Value> {
    let mut consumed_disappeared = HashSet::new();
    let mut events = Vec::with_capacity(fresh_notices.len());
    for notice in fresh_notices {
        let mut event = parse_notice_text_to_onebot_json(ctx, &notice.text, time_secs);
        enrich_single_notice_event(
            storage,
            ctx,
            disappeared_uids,
            &mut consumed_disappeared,
            &mut event,
        );
        events.push(event);
    }
    events
}

/// 对单条已分类 OneBot notice JSON 执行撤回数据库关联回填与删库，以及回应消息自发 ID 回填
pub fn enrich_notice_event(
    storage: &Storage,
    ctx: &NoticeContext<'_>,
    disappeared_uids: &[String],
    event: &mut Value,
) {
    let mut consumed = HashSet::new();
    enrich_single_notice_event(storage, ctx, disappeared_uids, &mut consumed, event);
}

fn enrich_single_notice_event(
    storage: &Storage,
    ctx: &NoticeContext<'_>,
    disappeared_uids: &[String],
    consumed_disappeared: &mut HashSet<String>,
    event: &mut Value,
) {
    let notice_type = event
        .get("notice_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let canon = canonical_group_name(ctx.session_name);
    let session_key = if canon.is_empty() {
        ctx.session_name.trim()
    } else {
        canon.as_str()
    };
    if session_key.is_empty() {
        return;
    }

    match notice_type {
        "group_recall" | "friend_recall" => {
            if event.get("message_id").and_then(Value::as_i64) != Some(0) {
                return;
            }
            let remaining_uids: Vec<String> = disappeared_uids
                .iter()
                .filter(|uid| !consumed_disappeared.contains(uid.as_str()))
                .cloned()
                .collect();
            let raw_notice = event
                .get("raw_notice")
                .and_then(Value::as_str)
                .unwrap_or("");
            let (sender_hint, actor_is_self) = extract_recall_hints(raw_notice, ctx.self_id, event);
            if let Ok(Some(recalled)) = storage.resolve_recalled_message_in_session(
                session_key,
                ctx.chat_kind,
                &remaining_uids,
                sender_hint.as_deref(),
                actor_is_self,
            ) {
                consumed_disappeared.insert(recalled.message_uid.clone());
                if let Some(obj) = event.as_object_mut() {
                    obj.insert("message_id".to_string(), json!(recalled.onebot_message_id));
                    if recalled.user_id > 0 {
                        obj.insert("user_id".to_string(), json!(recalled.user_id));
                    }
                }
                let _ = storage.delete_message_by_onebot_id(recalled.onebot_message_id);
            }
        }
        "group_msg_emoji_like" => {
            if event.get("message_id").and_then(Value::as_i64).unwrap_or(0) != 0 {
                return;
            }
            if let Ok(recent) = storage.list_recent_messages_by_kind(
                session_key,
                Some(common_types::ChatTargetKind::Group),
                30,
            ) {
                if let Some(target_self) = recent
                    .iter()
                    .rev()
                    .find(|msg| msg.is_self && msg.onebot_message_id > 0)
                {
                    if let Some(obj) = event.as_object_mut() {
                        obj.insert(
                            "message_id".to_string(),
                            json!(target_self.onebot_message_id),
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

fn extract_recall_hints(raw_notice: &str, self_id: i64, event: &Value) -> (Option<String>, bool) {
    let raw_actor = raw_notice
        .split_once("撤回了一条消息")
        .map(|(left, _)| {
            left.trim()
                .trim_matches(|ch: char| {
                    matches!(ch, '"' | '\'' | '“' | '”' | '‘' | '’' | '「' | '」')
                })
                .trim()
        })
        .unwrap_or("");
    let user_is_self = event
        .get("user_id")
        .and_then(Value::as_i64)
        .is_some_and(|uid| uid > 0 && uid == self_id);
    let actor_is_self = user_is_self || matches!(raw_actor, "你" | "我");
    let sender_hint = if raw_actor.is_empty() || matches!(raw_actor, "你" | "我" | "对方") {
        None
    } else {
        Some(raw_actor.to_string())
    };
    (sender_hint, actor_is_self)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common_types::{ChatTargetKind, GroupMemberInfo};
    use storage::RecordMessageInput;

    use super::super::tracker::MessageTracker;

    #[test]
    fn tracker_diff_and_notice_enricher_resolve_recall_delete_row_and_backfill_emoji_like() {
        let storage = Storage::open(":memory:").expect("memory db");
        let rec_self_id = storage
            .record_message(&RecordMessageInput {
                message_uid: "uid-self-1".to_string(),
                group_name: "Demo测试群".to_string(),
                group_id: 999888,
                user_id: 10001,
                sender_name: "测试机器人".to_string(),
                raw_message: "self msg".to_string(),
                is_self: true,
                timestamp_hint: Some("12:00".to_string()),
                captured_at_ms: 1_700_000_001_000,
                chat_type: ChatTargetKind::Group,
                segments: Vec::new(),
            })
            .expect("record self");
        let rec_peer_id = storage
            .record_message(&RecordMessageInput {
                message_uid: "uid-peer-2".to_string(),
                group_name: "Demo测试群".to_string(),
                group_id: 999888,
                user_id: 424242,
                sender_name: "群友B".to_string(),
                raw_message: "to be recalled".to_string(),
                is_self: false,
                timestamp_hint: Some("12:01".to_string()),
                captured_at_ms: 1_700_000_002_000,
                chat_type: ChatTargetKind::Group,
                segments: Vec::new(),
            })
            .expect("record peer");

        let mut tracker = MessageTracker::new(vec!["测试机器人".to_string()]);
        assert!(tracker
            .diff_visible_uids(
                "Demo测试群 (5)",
                &["uid-self-1".to_string(), "uid-peer-2".to_string()],
            )
            .is_empty());
        let disappeared = tracker.diff_visible_uids("Demo测试群", &["uid-self-1".to_string()]);
        assert_eq!(disappeared, vec!["uid-peer-2".to_string()]);

        let members = vec![GroupMemberInfo {
            group_id: 999888,
            user_id: 424242,
            real_user_id: Some(424242),
            nickname: "群友B".to_string(),
            card: String::new(),
            role: "member".to_string(),
        }];
        let ctx = NoticeContext {
            self_id: 10001,
            chat_kind: ChatTargetKind::Group,
            session_name: "Demo测试群 (5)",
            group_id: Some(999888),
            peer_user_id: None,
            members: &members,
        };
        let notices = vec![
            ParsedSystemNotice {
                notice_id: "notice-1".to_string(),
                text: "群友B 撤回了一条消息".to_string(),
            },
            ParsedSystemNotice {
                notice_id: "notice-2".to_string(),
                text: "群友B 回应了你的 消息:".to_string(),
            },
        ];

        let enriched =
            build_and_enrich_notice_events(&storage, &ctx, &notices, &disappeared, 1_700_000_005);
        assert_eq!(enriched.len(), 2);

        assert_eq!(enriched[0]["notice_type"], "group_recall");
        assert_eq!(enriched[0]["message_id"], rec_peer_id);
        assert_eq!(enriched[0]["user_id"], 424242);
        assert!(storage
            .get_message_by_id(rec_peer_id)
            .expect("query recalled")
            .is_none());

        assert_eq!(enriched[1]["notice_type"], "group_msg_emoji_like");
        assert_eq!(enriched[1]["message_id"], rec_self_id);

        // 验证私聊被动撤回同样能解析真实 message_id/user_id 并从 SQLite 清理记录
        let priv_msg_id = storage
            .record_message(&RecordMessageInput {
                message_uid: "uid-priv-1".to_string(),
                group_name: "测试好友".to_string(),
                group_id: 0,
                user_id: 778899,
                sender_name: "测试好友".to_string(),
                raw_message: "private hello".to_string(),
                is_self: false,
                timestamp_hint: Some("12:05".to_string()),
                captured_at_ms: 1_700_000_003_000,
                chat_type: ChatTargetKind::Private,
                segments: Vec::new(),
            })
            .expect("record private peer");
        assert!(tracker
            .diff_visible_uids("测试好友", &["uid-priv-1".to_string()])
            .is_empty());
        let priv_disappeared = tracker.diff_visible_uids("测试好友", &[]);
        assert_eq!(priv_disappeared, vec!["uid-priv-1".to_string()]);
        let priv_ctx = NoticeContext {
            self_id: 10001,
            chat_kind: ChatTargetKind::Private,
            session_name: "测试好友",
            group_id: None,
            peer_user_id: Some(778899),
            members: &[],
        };
        let priv_notices = vec![ParsedSystemNotice {
            notice_id: "notice-priv-1".to_string(),
            text: "对方 撤回了一条消息".to_string(),
        }];
        let priv_enriched = build_and_enrich_notice_events(
            &storage,
            &priv_ctx,
            &priv_notices,
            &priv_disappeared,
            1_700_000_009,
        );
        assert_eq!(priv_enriched[0]["notice_type"], "friend_recall");
        assert_eq!(priv_enriched[0]["message_id"], priv_msg_id);
        assert_eq!(priv_enriched[0]["user_id"], 778899);
        assert!(storage
            .get_message_by_id(priv_msg_id)
            .expect("query recalled private")
            .is_none());
    }

    #[test]
    fn recall_and_emoji_like_isolate_same_name_group_and_private_sessions() {
        let storage = Storage::open(":memory:").expect("open mem db");
        let group_self_id = storage
            .record_message(&RecordMessageInput {
                message_uid: "uid-same-group-self".to_string(),
                group_name: "同名会话".to_string(),
                group_id: 666777,
                user_id: 10001,
                sender_name: "BotSelf".to_string(),
                raw_message: "same session group self".to_string(),
                is_self: true,
                timestamp_hint: Some("12:10".to_string()),
                captured_at_ms: 1_700_000_010_000,
                chat_type: ChatTargetKind::Group,
                segments: Vec::new(),
            })
            .expect("record group self");
        let priv_self_id = storage
            .record_message(&RecordMessageInput {
                message_uid: "uid-same-priv-self".to_string(),
                group_name: "同名会话".to_string(),
                group_id: 0,
                user_id: 889900,
                sender_name: "BotSelf".to_string(),
                raw_message: "same session priv self".to_string(),
                is_self: true,
                timestamp_hint: Some("12:11".to_string()),
                captured_at_ms: 1_700_000_011_000,
                chat_type: ChatTargetKind::Private,
                segments: Vec::new(),
            })
            .expect("record private self");
        let priv_peer_id = storage
            .record_message(&RecordMessageInput {
                message_uid: "uid-same-priv-peer".to_string(),
                group_name: "同名会话".to_string(),
                group_id: 0,
                user_id: 889900,
                sender_name: "同名会话".to_string(),
                raw_message: "same session priv peer".to_string(),
                is_self: false,
                timestamp_hint: Some("12:12".to_string()),
                captured_at_ms: 1_700_000_012_000,
                chat_type: ChatTargetKind::Private,
                segments: Vec::new(),
            })
            .expect("record private peer");

        // 同名私聊中发生被动撤回与群表情回应时，不得越权删改或命中同名群消息
        let mut tracker = MessageTracker::new(vec![]);
        assert!(tracker
            .diff_visible_uids_for(
                ChatTargetKind::Private,
                "同名会话",
                &["uid-same-priv-peer".to_string()],
            )
            .is_empty());
        let disappeared = tracker.diff_visible_uids_for(ChatTargetKind::Private, "同名会话", &[]);
        assert_eq!(disappeared, vec!["uid-same-priv-peer".to_string()]);

        let priv_ctx = NoticeContext {
            self_id: 10001,
            chat_kind: ChatTargetKind::Private,
            session_name: "同名会话",
            group_id: None,
            peer_user_id: Some(889900),
            members: &[],
        };
        let priv_notices = vec![ParsedSystemNotice {
            notice_id: "n-same-priv".to_string(),
            text: "对方 撤回了一条消息".to_string(),
        }];
        let priv_events = build_and_enrich_notice_events(
            &storage,
            &priv_ctx,
            &priv_notices,
            &disappeared,
            1_700_000_015,
        );
        assert_eq!(priv_events[0]["notice_type"], "friend_recall");
        assert_eq!(priv_events[0]["message_id"], priv_peer_id);
        assert!(storage
            .get_message_by_id(group_self_id)
            .expect("group self must stay untouched")
            .is_some());
        assert!(storage
            .get_message_by_id(priv_self_id)
            .expect("private self must stay untouched")
            .is_some());

        // 同名群聊收到表态回应时，应仅回查群内自发消息 ID，忽略更新的私聊自发消息
        let members = vec![GroupMemberInfo {
            group_id: 666777,
            user_id: 334455,
            real_user_id: Some(334455),
            nickname: "群友C".to_string(),
            card: "群友C".to_string(),
            role: "member".to_string(),
        }];
        let group_ctx = NoticeContext {
            self_id: 10001,
            chat_kind: ChatTargetKind::Group,
            session_name: "同名会话",
            group_id: Some(666777),
            peer_user_id: None,
            members: &members,
        };
        let emoji_notices = vec![ParsedSystemNotice {
            notice_id: "n-same-emoji".to_string(),
            text: "群友C 回应了你的 消息:".to_string(),
        }];
        let group_events = build_and_enrich_notice_events(
            &storage,
            &group_ctx,
            &emoji_notices,
            &[],
            1_700_000_016,
        );
        assert_eq!(group_events[0]["notice_type"], "group_msg_emoji_like");
        assert_eq!(group_events[0]["message_id"], group_self_id);
    }
}
