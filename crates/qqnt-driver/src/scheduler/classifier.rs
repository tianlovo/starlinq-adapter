//! 机器人相关性消息分层器：将消息划分为 P0 关键触发、P1 上下文活跃与 P2 普通闲聊并计算响应截止时间

use std::collections::HashMap;

use common_types::{
    ChatMessage, ChatTargetKind, MessagePriorityTier, MessageSegment, SchedulerConfig,
};

use crate::parser::canonical_group_name;

/// 分类计算所需的机器人身份与触发规则上下文
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClassifyContext<'a> {
    pub(crate) self_id: u64,
    pub(crate) self_nicknames: &'a [String],
    pub(crate) config: &'a SchedulerConfig,
}

/// 依据机器人提及、引用关系、命令前缀与活跃对话线程对入站消息进行优先级分类
#[derive(Debug, Default)]
pub(crate) struct BotRelevanceClassifier {
    active_threads: HashMap<String, u64>,
}

impl BotRelevanceClassifier {
    /// 创建一个带空活跃线程表的新消息优先级分类器
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 对单条消息执行优先级分类并同步刷新对应群聊的 P1 活跃线程生存期
    pub(crate) fn classify(
        &mut self,
        now_ms: u64,
        msg: &ChatMessage,
        ctx: &ClassifyContext<'_>,
    ) -> (MessagePriorityTier, u64) {
        self.prune_expired_threads(now_ms);
        let tier = self.evaluate_tier(now_ms, msg, ctx);
        if tier == MessagePriorityTier::CriticalBotTrigger
            && msg.target_kind == ChatTargetKind::Group
        {
            let canon = canonical_group_name(&msg.group_name);
            if !canon.is_empty() {
                let until = now_ms.saturating_add(ctx.config.active_thread_ttl_ms);
                self.active_threads.insert(canon, until);
            }
        }
        let deadline = compute_response_deadline_ms(now_ms, msg.captured_at_ms, tier, ctx.config);
        (tier, deadline)
    }

    /// 仅评估当前状态下的优先级与截止时间，不修改活跃线程状态表
    pub(crate) fn peek_tier(
        &self,
        now_ms: u64,
        msg: &ChatMessage,
        ctx: &ClassifyContext<'_>,
    ) -> MessagePriorityTier {
        self.evaluate_tier(now_ms, msg, ctx)
    }

    /// 根据 P0、P1、P2 优先级规则判定单条消息所属层级
    fn evaluate_tier(
        &self,
        now_ms: u64,
        msg: &ChatMessage,
        ctx: &ClassifyContext<'_>,
    ) -> MessagePriorityTier {
        if msg.target_kind == ChatTargetKind::Private || msg.reply_targets_self {
            return MessagePriorityTier::CriticalBotTrigger;
        }

        let plain_text = extract_effective_plain_text(msg);
        if segments_mention_self(&msg.segments, ctx.self_id, ctx.self_nicknames)
            || text_mentions_self(&plain_text, ctx.self_id, ctx.self_nicknames)
            || text_mentions_self(&msg.content, ctx.self_id, ctx.self_nicknames)
            || matches_command_prefix(&plain_text, &ctx.config.command_prefixes)
            || matches_wake_keyword(&plain_text, &msg.content, &ctx.config.wake_keywords)
        {
            return MessagePriorityTier::CriticalBotTrigger;
        }

        let canon = canonical_group_name(&msg.group_name);
        let has_active_thread = self
            .active_threads
            .get(&canon)
            .is_some_and(|&until| now_ms <= until);
        let is_reply_followup = msg.reply_to_message_id.is_some()
            || msg
                .segments
                .iter()
                .any(|seg| matches!(seg, MessageSegment::Reply { .. }));
        if segments_mention_all(&msg.segments)
            || text_mentions_all(&plain_text)
            || text_mentions_all(&msg.content)
            || has_active_thread
            || is_reply_followup
        {
            return MessagePriorityTier::HighContextual;
        }

        MessagePriorityTier::OrdinaryChatter
    }

    /// 清理已过期的活跃群聊对话线程条目，防止长期运行后内存持续增长
    fn prune_expired_threads(&mut self, now_ms: u64) {
        self.active_threads.retain(|_, &mut until| now_ms <= until);
    }
}

/// 根据消息所属分层与采集时间戳计算响应截止时间（毫秒）
pub(crate) fn compute_response_deadline_ms(
    now_ms: u64,
    captured_at_ms: u64,
    tier: MessagePriorityTier,
    cfg: &SchedulerConfig,
) -> u64 {
    let base_ts = if captured_at_ms > 0 {
        captured_at_ms
    } else {
        now_ms
    };
    let window_ms = match tier {
        MessagePriorityTier::CriticalBotTrigger => cfg.critical_response_window_ms,
        MessagePriorityTier::HighContextual => cfg.contextual_response_window_ms,
        MessagePriorityTier::OrdinaryChatter => cfg.ordinary_response_window_ms,
    };
    base_ts.saturating_add(window_ms)
}

/// 提取用于命令前缀与正文提及检测的有效纯文本（优先拼接文本段，回退到消息正文）
fn extract_effective_plain_text(msg: &ChatMessage) -> String {
    let mut joined = String::new();
    for seg in &msg.segments {
        if let MessageSegment::Text { text } = seg {
            joined.push_str(text);
        }
    }
    if !joined.trim().is_empty() {
        joined
    } else {
        msg.content.clone()
    }
}

/// 判断结构化 At 消息段是否明确指向当前机器人账号或昵称
fn segments_mention_self(
    segments: &[MessageSegment],
    self_id: u64,
    self_nicknames: &[String],
) -> bool {
    let self_id_str = (self_id > 0).then(|| self_id.to_string());
    segments.iter().any(|seg| match seg {
        MessageSegment::At { qq, name } => {
            if qq.eq_ignore_ascii_case("all") {
                return false;
            }
            if let Some(ref sid) = self_id_str {
                if qq.trim() == sid {
                    return true;
                }
            }
            nick_matches_self_list(qq, self_nicknames)
                || name
                    .as_deref()
                    .is_some_and(|n| nick_matches_self_list(n, self_nicknames))
        }
        _ => false,
    })
}

/// 判断结构化 At 消息段是否包含 @全体成员
fn segments_mention_all(segments: &[MessageSegment]) -> bool {
    segments.iter().any(|seg| match seg {
        MessageSegment::At { qq, name } => {
            qq.eq_ignore_ascii_case("all")
                || name.as_deref().is_some_and(|n| n.trim() == "全体成员")
        }
        _ => false,
    })
}

/// 检查文本中是否包含指向机器人昵称或 QQ 号的 @ 提及
fn text_mentions_self(text: &str, self_id: u64, self_nicknames: &[String]) -> bool {
    if text.is_empty() {
        return false;
    }
    let lower = text.to_lowercase();
    let self_id_str = (self_id > 0).then(|| self_id.to_string());
    for (idx, ch) in lower.char_indices() {
        if ch != '@' && ch != '＠' {
            continue;
        }
        let rest = &lower[idx + ch.len_utf8()..];
        if let Some(ref sid) = self_id_str {
            if starts_with_token_boundary(rest, sid) {
                return true;
            }
        }
        if self_nicknames.iter().any(|nick| {
            let trimmed = nick.trim();
            !trimmed.is_empty() && starts_with_token_boundary(rest, &trimmed.to_lowercase())
        }) {
            return true;
        }
    }
    false
}

/// 检查文本中是否包含 @全体成员 或 ＠全体成员 提及
fn text_mentions_all(text: &str) -> bool {
    text.contains("@全体成员") || text.contains("＠全体成员")
}

/// 判断前缀匹配后的紧邻字符是否为分词边界，避免 `@bot` 误命中 `@bot2`
fn starts_with_token_boundary(rest_lower: &str, needle_lower: &str) -> bool {
    if !rest_lower.starts_with(needle_lower) {
        return false;
    }
    match rest_lower[needle_lower.len()..].chars().next() {
        None => true,
        Some(next_ch) => !next_ch.is_alphanumeric() && next_ch != '_' && next_ch != '-',
    }
}

/// 判断单个名称是否匹配已配置的机器人自昵称列表
fn nick_matches_self_list(candidate: &str, self_nicknames: &[String]) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return false;
    }
    self_nicknames.iter().any(|nick| {
        let n = nick.trim();
        !n.is_empty() && n.eq_ignore_ascii_case(trimmed)
    })
}

/// 判断文本是否以合法命令前缀开头，并安全排除 `..` / `...` 等省略号误判
fn matches_command_prefix(text: &str, command_prefixes: &[String]) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    for prefix in command_prefixes {
        if prefix.is_empty() || !trimmed.starts_with(prefix.as_str()) {
            continue;
        }
        let after = &trimmed[prefix.len()..];
        if after.trim().is_empty() {
            continue;
        }
        if (prefix == "." || prefix == "。") && (after.starts_with('.') || after.starts_with('。'))
        {
            continue;
        }
        return true;
    }
    false
}

/// 判断消息正文是否命中配置中的任意唤醒关键词（忽略大小写）
fn matches_wake_keyword(plain_text: &str, raw_content: &str, wake_keywords: &[String]) -> bool {
    if wake_keywords.is_empty() {
        return false;
    }
    let plain_lower = plain_text.to_lowercase();
    let raw_lower = raw_content.to_lowercase();
    wake_keywords.iter().any(|kw| {
        let trimmed = kw.trim();
        if trimmed.is_empty() {
            return false;
        }
        let needle = trimmed.to_lowercase();
        plain_lower.contains(&needle) || raw_lower.contains(&needle)
    })
}
