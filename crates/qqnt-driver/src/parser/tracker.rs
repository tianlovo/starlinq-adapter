//! 消息增量追踪器、30 秒本端发送回显防循环抑制及出站占位 ID 重绑定实现

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use common_types::constants::IMAGE_PLACEHOLDER_TOKEN;
use common_types::{ChatMessage, ChatTargetKind, MessageSegment};

use super::notices::ParsedSystemNotice;
use super::row_cleaner::{canonical_group_name, default_bot_self_name, normalize_message_text};
use crate::constants::{
    FILE_BUBBLE_SIZE_REGEX, FILE_BUBBLE_STATUS_KEYWORDS, MESSAGE_TRACKER_MAX_OUTBOUND,
    MESSAGE_TRACKER_MAX_SEEN_IDS, OUTBOUND_TRACKER_TTL,
};

/// 记录本端近期发出文本用于 30 秒防回显循环匹配的缓冲条目
#[derive(Debug, Clone)]
struct OutboundEntry {
    normalized: String,
    pub session_key: String,
    sent_at: Instant,
}

/// 记录已持久化出站占位消息与规范化正文之间的回显重绑定凭证
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutboundStubBinding {
    pub onebot_message_id: i64,
    pub stub_uid: String,
    pub norm_text: String,
    pub media_token: Option<String>,
    pub session_key: String,
}

/// 维护有界已见消息 ID 队列、已确认自发消息集合、UI UID 重绑定映射与本端出站防循环缓存
#[derive(Debug)]
pub struct MessageTracker {
    self_nicknames: Vec<String>,
    seen_ids: HashSet<String>,
    confirmed_self_ids: HashSet<String>,
    seen_order: VecDeque<String>,
    seen_notice_ids: HashSet<String>,
    seen_notice_order: VecDeque<String>,
    outbound_ring: VecDeque<OutboundEntry>,
    pending_stub_bindings: VecDeque<(OutboundStubBinding, Instant)>,
    recent_self_media_tokens: VecDeque<(String, i64, String, Instant)>,
    uid_aliases: HashMap<String, (i64, String, String)>,
    uid_alias_order: VecDeque<String>,
    loose_uid_aliases: HashMap<(String, String, String, usize), String>,
    loose_uid_order: VecDeque<(String, String, String, usize)>,
    seeded_sessions: HashSet<String>,
    confirmed_self_onebot_ids: HashSet<i64>,
    confirmed_self_onebot_order: VecDeque<i64>,
    visible_uids_by_session: HashMap<String, Vec<String>>,
}

/// 构造带会话类型前缀的标准化追踪器会话键（`group:<name>` 或 `private:<name>`，空会话返回空串）
pub fn tracker_session_key(kind: ChatTargetKind, session: &str) -> String {
    let canon = canonical_group_name(session);
    let norm = if canon.is_empty() {
        session.trim()
    } else {
        canon.as_str()
    };
    if norm.is_empty() {
        String::new()
    } else {
        format!("{}:{norm}", kind.as_str())
    }
}

fn session_keys_compatible(entry_key: &str, query_key: &str) -> bool {
    entry_key.is_empty() || query_key.is_empty() || entry_key == query_key
}

impl MessageTracker {
    /// 创建消息追踪器并预设机器人已知自账号昵称（默认包含 `"BotSelf"`）
    pub fn new(self_nicknames: Vec<String>) -> Self {
        let mut tracker = Self {
            self_nicknames: vec![default_bot_self_name().to_string()],
            seen_ids: HashSet::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            confirmed_self_ids: HashSet::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            seen_order: VecDeque::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            seen_notice_ids: HashSet::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            seen_notice_order: VecDeque::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            outbound_ring: VecDeque::new(),
            pending_stub_bindings: VecDeque::new(),
            recent_self_media_tokens: VecDeque::new(),
            uid_aliases: HashMap::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            uid_alias_order: VecDeque::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            loose_uid_aliases: HashMap::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            loose_uid_order: VecDeque::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            seeded_sessions: HashSet::new(),
            confirmed_self_onebot_ids: HashSet::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            confirmed_self_onebot_order: VecDeque::with_capacity(MESSAGE_TRACKER_MAX_SEEN_IDS),
            visible_uids_by_session: HashMap::new(),
        };
        tracker.add_self_nicknames(&self_nicknames);
        tracker
    }

    /// 增量注册界面被动发现的当前登录 QQ 自账号昵称（自动去重）
    pub fn add_self_nicknames(&mut self, nicks: &[String]) {
        for nick in nicks {
            let trimmed = nick.trim();
            if !trimmed.is_empty()
                && !self
                    .self_nicknames
                    .iter()
                    .any(|existing| existing == trimmed)
            {
                self.self_nicknames.push(trimmed.to_string());
            }
        }
    }

    /// 返回当前已知自账号昵称列表（按注册顺序去重）
    pub fn self_nicknames(&self) -> &[String] {
        &self.self_nicknames
    }

    /// 向 30 秒防自回复循环缓冲区压入本端即将或刚刚发出的正文文本（无会话通配，仅供兼容单测）
    pub fn record_outbound(&mut self, text: &str) {
        self.record_outbound_with_key(String::new(), text);
    }

    /// 向 30 秒防自回复循环缓冲区压入绑定特定会话类型与名称的本端发出正文
    pub fn record_outbound_for_session(&mut self, kind: ChatTargetKind, session: &str, text: &str) {
        self.record_outbound_with_key(tracker_session_key(kind, session), text);
    }

    fn record_outbound_with_key(&mut self, session_key: String, text: &str) {
        self.prune_outbound();
        let normalized = normalize_message_text(text);
        if normalized.is_empty() {
            return;
        }
        while self.outbound_ring.len() >= MESSAGE_TRACKER_MAX_OUTBOUND {
            self.outbound_ring.pop_front();
        }
        self.outbound_ring.push_back(OutboundEntry {
            normalized,
            session_key,
            sent_at: Instant::now(),
        });
    }

    /// 当出站发送在提交前中止时，回滚最近一条匹配的防循环记录（无会话通配，仅供兼容单测）
    pub fn rollback_outbound(&mut self, text: &str) {
        self.rollback_outbound_with_key("", text);
    }

    /// 当出站发送在提交前中止时，按会话类型与会话名回滚最近一条匹配的防循环记录
    pub fn rollback_outbound_for_session(
        &mut self,
        kind: ChatTargetKind,
        session: &str,
        text: &str,
    ) {
        let key = tracker_session_key(kind, session);
        self.rollback_outbound_with_key(&key, text);
    }

    fn rollback_outbound_with_key(&mut self, target_key: &str, text: &str) {
        let normalized = normalize_message_text(text);
        let pos = if !target_key.is_empty() {
            self.outbound_ring
                .iter()
                .rposition(|entry| {
                    entry.session_key == target_key && entry.normalized == normalized
                })
                .or_else(|| {
                    self.outbound_ring.iter().rposition(|entry| {
                        session_keys_compatible(&entry.session_key, target_key)
                            && entry.normalized == normalized
                    })
                })
        } else {
            self.outbound_ring
                .iter()
                .rposition(|entry| entry.normalized == normalized)
        };
        if let Some(pos) = pos {
            self.outbound_ring.remove(pos);
        }
    }

    /// 注册已完成持久化的本端出站占位消息，等待后续 UIA 轮询真实回显 UID 重绑定
    pub fn register_confirmed_outbound(&mut self, mut binding: OutboundStubBinding) {
        self.record_confirmed_self_onebot_id(binding.onebot_message_id);
        self.prune_outbound();
        let norm = normalize_message_text(&binding.norm_text);
        binding.media_token = binding
            .media_token
            .as_deref()
            .and_then(normalize_media_token);
        if norm.is_empty() && binding.media_token.is_none() {
            return;
        }
        binding.norm_text = norm;
        if let Some(ref token) = binding.media_token {
            if !self
                .recent_self_media_tokens
                .iter()
                .any(|(_, id, _, _)| *id == binding.onebot_message_id)
            {
                while self.recent_self_media_tokens.len() >= MESSAGE_TRACKER_MAX_OUTBOUND {
                    self.recent_self_media_tokens.pop_front();
                }
                self.recent_self_media_tokens.push_back((
                    token.clone(),
                    binding.onebot_message_id,
                    binding.session_key.clone(),
                    Instant::now(),
                ));
            }
        }
        while self.pending_stub_bindings.len() >= MESSAGE_TRACKER_MAX_OUTBOUND {
            self.pending_stub_bindings.pop_front();
        }
        self.pending_stub_bindings
            .push_back((binding, Instant::now()));
    }

    /// 将轮询采集到的本端回显真实 UI UID 与先前登记的出站占位绑定凭证对齐，返回 `(onebot_id, ui_uid, timestamp_hint)`
    pub fn reconcile_polled_self_messages(
        &mut self,
        messages: &mut [ChatMessage],
    ) -> Vec<(i64, String, Option<String>)> {
        self.prune_outbound();
        self.stabilize_batch_uids(messages);
        let mut rebound = Vec::new();
        let mut claimed_onebot_ids_in_batch: HashSet<i64> = HashSet::new();
        let mut handled_indices = vec![false; messages.len()];

        // 第一遍：先恢复本轮快照中已有 UI UID 别名的消息并锁定对应 OneBot ID，避免第二遍重复抢占
        for (idx, msg) in messages.iter_mut().enumerate() {
            let ui_uid = msg.message_id.clone();
            let msg_session_key = tracker_session_key(msg.target_kind, &msg.group_name);
            if let Some(&(onebot_id, _, ref alias_sk)) = self.uid_aliases.get(&ui_uid) {
                if session_keys_compatible(alias_sk, &msg_session_key)
                    && !claimed_onebot_ids_in_batch.contains(&onebot_id)
                {
                    claimed_onebot_ids_in_batch.insert(onebot_id);
                    msg.is_self = true;
                    msg.onebot_message_id = Some(onebot_id);
                    self.record_confirmed_self_onebot_id(onebot_id);
                    self.insert_seen_id(ui_uid.clone());
                    self.confirmed_self_ids.insert(ui_uid.clone());
                    rebound.push((onebot_id, ui_uid, msg.timestamp_hint.clone()));
                    handled_indices[idx] = true;
                }
            }
        }

        // 第二遍：倒序（从最新到最旧）仅处理尚未见过的气泡，先匹配明确自发气泡再回退到正文防回显命中
        for allow_peek_fallback in [false, true] {
            for (idx, msg) in messages.iter_mut().enumerate().rev() {
                if handled_indices[idx] {
                    continue;
                }
                let ui_uid = msg.message_id.clone();
                if self.seen_ids.contains(&ui_uid) {
                    continue;
                }
                let msg_session_key = tracker_session_key(msg.target_kind, &msg.group_name);
                let confirmed_self = msg.is_self
                    || self.confirmed_self_ids.contains(&ui_uid)
                    || self.matches_self_nickname(&msg.sender);
                let is_polled_self = confirmed_self
                    || (allow_peek_fallback
                        && !self.is_definite_peer_message(msg)
                        && self.peek_outbound_for_session(&msg_session_key, &msg.content));
                if !is_polled_self {
                    continue;
                }
                if let Some(hit) =
                    self.try_reconcile_unseen_polled_msg(msg, &mut claimed_onebot_ids_in_batch)
                {
                    handled_indices[idx] = true;
                    rebound.push(hit);
                }
            }
        }
        rebound
    }

    fn try_reconcile_unseen_polled_msg(
        &mut self,
        msg: &mut ChatMessage,
        claimed_onebot_ids_in_batch: &mut HashSet<i64>,
    ) -> Option<(i64, String, Option<String>)> {
        let msg_session_key = tracker_session_key(msg.target_kind, &msg.group_name);
        let norm = normalize_message_text(&msg.content);
        if norm.is_empty() {
            return None;
        }
        if let Some(pos) = self.pending_stub_bindings.iter().rposition(|(entry, _)| {
            !claimed_onebot_ids_in_batch.contains(&entry.onebot_message_id)
                && session_keys_compatible(&entry.session_key, &msg_session_key)
                && entry.norm_text == norm
        }) {
            if let Some((binding, _)) = self.pending_stub_bindings.remove(pos) {
                return Some(self.commit_polled_rebind(
                    msg,
                    binding.onebot_message_id,
                    binding.stub_uid,
                    &msg_session_key,
                    claimed_onebot_ids_in_batch,
                ));
            }
        }

        let cand_name = extract_polled_media_token(msg)?;
        if let Some(pos) = self.pending_stub_bindings.iter().rposition(|(entry, _)| {
            !claimed_onebot_ids_in_batch.contains(&entry.onebot_message_id)
                && session_keys_compatible(&entry.session_key, &msg_session_key)
                && entry.media_token.as_deref() == Some(cand_name.as_str())
        }) {
            if let Some((binding, _)) = self.pending_stub_bindings.remove(pos) {
                return Some(self.commit_polled_rebind(
                    msg,
                    binding.onebot_message_id,
                    binding.stub_uid,
                    &msg_session_key,
                    claimed_onebot_ids_in_batch,
                ));
            }
        }

        let onebot_id = self
            .recent_self_media_tokens
            .iter()
            .rev()
            .find(|(tok, id, sk, _)| {
                tok == &cand_name
                    && !claimed_onebot_ids_in_batch.contains(id)
                    && session_keys_compatible(sk, &msg_session_key)
            })
            .map(|(_, id, _, _)| *id)?;
        let stub_uid = self
            .uid_aliases
            .values()
            .find(|(id, _, _)| *id == onebot_id)
            .map(|(_, s, _)| s.clone())
            .unwrap_or_else(|| format!("media-stub-{onebot_id}"));
        Some(self.commit_polled_rebind(
            msg,
            onebot_id,
            stub_uid,
            &msg_session_key,
            claimed_onebot_ids_in_batch,
        ))
    }

    fn commit_polled_rebind(
        &mut self,
        msg: &mut ChatMessage,
        onebot_id: i64,
        stub_uid: String,
        msg_session_key: &str,
        claimed_onebot_ids_in_batch: &mut HashSet<i64>,
    ) -> (i64, String, Option<String>) {
        let ui_uid = msg.message_id.clone();
        self.pending_stub_bindings
            .retain(|(e, _)| e.onebot_message_id != onebot_id);
        claimed_onebot_ids_in_batch.insert(onebot_id);
        let _ = self.consume_first_outbound_for_session(msg_session_key, &msg.content);
        self.record_confirmed_self_onebot_id(onebot_id);
        self.insert_uid_alias(
            ui_uid.clone(),
            onebot_id,
            stub_uid,
            msg_session_key.to_string(),
        );
        self.insert_seen_id(ui_uid.clone());
        self.confirmed_self_ids.insert(ui_uid.clone());
        msg.is_self = true;
        msg.onebot_message_id = Some(onebot_id);
        (onebot_id, ui_uid, msg.timestamp_hint.clone())
    }

    /// 打开群聊时植入当前历史消息基准 ID，防止存量历史消息触发新消息事件
    pub fn seed_baseline(&mut self, messages: &[ChatMessage]) {
        self.prune_outbound();
        let mut occ_counts: HashMap<(String, String, String), usize> =
            HashMap::with_capacity(messages.len());
        for msg in messages {
            let session_key = tracker_session_key(msg.target_kind, &msg.group_name);
            if !session_key.is_empty() {
                self.seeded_sessions.insert(session_key);
            }
            if msg.is_self {
                if let Some(onebot_id) = msg.onebot_message_id {
                    self.record_confirmed_self_onebot_id(onebot_id);
                }
            }
            let canonical_uid = self.resolve_or_register_loose_uid(msg, &mut occ_counts);
            self.insert_seen_id(canonical_uid);
        }
    }

    /// 显式记录目标会话已完成基线播种（包含空历史消息会话）
    pub fn mark_session_seeded(&mut self, session: &str) {
        let canon = canonical_group_name(session);
        if !canon.is_empty() {
            self.seeded_sessions.insert(canon);
        } else if !session.trim().is_empty() {
            self.seeded_sessions.insert(session.trim().to_string());
        }
    }

    /// 按聊天类型与会话名称显式记录目标会话已完成基线播种（隔离同名群聊与私聊）
    pub fn mark_session_seeded_for(&mut self, kind: ChatTargetKind, session: &str) {
        let key = tracker_session_key(kind, session);
        if !key.is_empty() {
            self.seeded_sessions.insert(key);
        }
    }

    /// 判断目标会话是否已在追踪器中建立过历史基线，决定切换后应增量轮询还是重新播种
    pub fn is_seeded(&self, session: &str) -> bool {
        let canon = canonical_group_name(session);
        let group_key = tracker_session_key(ChatTargetKind::Group, session);
        let private_key = tracker_session_key(ChatTargetKind::Private, session);
        (!group_key.is_empty() && self.seeded_sessions.contains(&group_key))
            || (!private_key.is_empty() && self.seeded_sessions.contains(&private_key))
            || (!canon.is_empty() && self.seeded_sessions.contains(&canon))
            || self.seeded_sessions.contains(session.trim())
    }

    /// 按聊天类型与会话名称判断目标会话是否已建立历史基线（隔离同名群聊与私聊）
    pub fn is_seeded_for(&self, kind: ChatTargetKind, session: &str) -> bool {
        let key = tracker_session_key(kind, session);
        !key.is_empty() && self.seeded_sessions.contains(&key)
    }

    /// 记录最近已确认属于机器人本端的 OneBot 消息 ID（有界队列），用于入站回复归属判定
    pub fn record_confirmed_self_onebot_id(&mut self, onebot_id: i64) {
        if onebot_id <= 0 {
            return;
        }
        if self.confirmed_self_onebot_ids.insert(onebot_id) {
            self.confirmed_self_onebot_order.push_back(onebot_id);
            while self.confirmed_self_onebot_order.len() > MESSAGE_TRACKER_MAX_SEEN_IDS {
                if let Some(evicted) = self.confirmed_self_onebot_order.pop_front() {
                    self.confirmed_self_onebot_ids.remove(&evicted);
                }
            }
        }
    }

    /// 判断指定 OneBot 消息 ID 是否属于近期已确认的机器人自发消息
    pub fn is_confirmed_self_onebot_id(&self, onebot_id: i64) -> bool {
        onebot_id > 0 && self.confirmed_self_onebot_ids.contains(&onebot_id)
    }

    /// 对比同一会话上一轮可见消息 UID 列表与本轮最新列表，返回已从视野中消失的 UID（用于被动撤回定位）
    pub fn diff_visible_uids(
        &mut self,
        session_name: &str,
        current_uids: &[String],
    ) -> Vec<String> {
        let canon = canonical_group_name(session_name);
        let key = if canon.is_empty() {
            session_name.trim().to_string()
        } else {
            canon
        };
        if key.is_empty() {
            return Vec::new();
        }
        self.diff_visible_uids_by_key(key, current_uids)
    }

    /// 按聊天类型与会话名称隔离对比可见消息 UID 列表，返回从视野中消失的 UID
    pub fn diff_visible_uids_for(
        &mut self,
        kind: ChatTargetKind,
        session_name: &str,
        current_uids: &[String],
    ) -> Vec<String> {
        let key = tracker_session_key(kind, session_name);
        if key.is_empty() {
            return Vec::new();
        }
        self.diff_visible_uids_by_key(key, current_uids)
    }

    fn diff_visible_uids_by_key(&mut self, key: String, current_uids: &[String]) -> Vec<String> {
        let disappeared = match self.visible_uids_by_session.get(&key) {
            Some(prev) => {
                let current_set: HashSet<&str> = current_uids.iter().map(String::as_str).collect();
                prev.iter()
                    .filter(|uid| !current_set.contains(uid.as_str()))
                    .cloned()
                    .collect()
            }
            None => Vec::new(),
        };
        self.visible_uids_by_session
            .insert(key, current_uids.to_vec());
        disappeared
    }

    /// 判断消息是否明确属于非己方群友或对方联系人（已注册真实自昵称且发送者具名且非自账号）
    pub fn is_definite_peer_message(&self, msg: &ChatMessage) -> bool {
        if msg.is_self {
            return false;
        }
        let has_real_self_nick = self.self_nicknames.iter().any(|nick| {
            let trimmed = nick.trim();
            !trimmed.is_empty() && trimmed != default_bot_self_name()
        });
        if !has_real_self_nick {
            return false;
        }
        let sender_trimmed = msg.sender.trim();
        !sender_trimmed.is_empty()
            && !sender_trimmed.eq_ignore_ascii_case("unknown")
            && sender_trimmed != default_bot_self_name()
            && !self.matches_self_nickname(sender_trimmed)
    }

    /// 标注每条消息的 `is_self` 标记并返回 `(全量已标注列表, 新增群友入站列表)`
    pub fn observe_poll(
        &mut self,
        mut messages: Vec<ChatMessage>,
    ) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
        self.prune_outbound();
        self.stabilize_batch_uids(&mut messages);
        let mut new_incoming = Vec::new();

        for msg in &mut messages {
            let msg_session_key = tracker_session_key(msg.target_kind, &msg.group_name);
            let alias_onebot_id = self
                .uid_aliases
                .get(&msg.message_id)
                .filter(|(_, _, alias_sk)| session_keys_compatible(alias_sk, &msg_session_key))
                .map(|(id, _, _)| *id);
            if let Some(onebot_id) = alias_onebot_id {
                msg.onebot_message_id = Some(onebot_id);
            }
            let already_confirmed =
                self.confirmed_self_ids.contains(&msg.message_id) || alias_onebot_id.is_some();
            let nick_or_align_self = msg.is_self || self.matches_self_nickname(&msg.sender);
            let definite_peer = self.is_definite_peer_message(msg);
            let is_new = self.insert_seen_id(msg.message_id.clone());

            let is_self = if already_confirmed || nick_or_align_self {
                if is_new {
                    let _ = self.consume_first_outbound_for_session(&msg_session_key, &msg.content);
                }
                true
            } else if is_new && !definite_peer {
                self.consume_first_outbound_for_session(&msg_session_key, &msg.content)
            } else {
                false
            };

            msg.is_self = is_self;
            if msg.is_self {
                self.confirmed_self_ids.insert(msg.message_id.clone());
            } else if is_new {
                new_incoming.push(msg.clone());
            }
        }

        (messages, new_incoming)
    }

    /// 仅标注 `is_self` 并应用已重绑定占位 UID 改写与去重，而不修改 `seen_ids` 或消费出站令牌
    pub fn annotate_only(&mut self, mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
        self.prune_outbound();
        self.stabilize_batch_uids(&mut messages);
        let mut seen_out = HashSet::with_capacity(messages.len());
        let mut deduped = Vec::with_capacity(messages.len());
        for mut msg in messages {
            let msg_session_key = tracker_session_key(msg.target_kind, &msg.group_name);
            let matched_alias = self
                .uid_aliases
                .get(&msg.message_id)
                .filter(|(_, _, alias_sk)| session_keys_compatible(alias_sk, &msg_session_key))
                .map(|(id, stub, _)| (*id, stub.clone()));
            if let Some((onebot_id, stub_uid)) = matched_alias {
                msg.is_self = true;
                msg.onebot_message_id = Some(onebot_id);
                msg.message_id = stub_uid;
            } else {
                let definite_peer = self.is_definite_peer_message(&msg);
                msg.is_self = msg.is_self
                    || self.confirmed_self_ids.contains(&msg.message_id)
                    || self.matches_self_nickname(&msg.sender)
                    || (!definite_peer
                        && (self.peek_outbound_for_session(&msg_session_key, &msg.content)
                            || self.peek_pending_stub_for_session(&msg_session_key, &msg.content)));
            }
            if seen_out.insert(msg.message_id.clone()) {
                deduped.push(msg);
            }
        }
        deduped
    }

    /// 返回当前已缓存的去重消息 ID 数量（测试断言上限不突破 `MESSAGE_TRACKER_MAX_SEEN_IDS`）
    #[cfg(test)]
    pub(crate) fn seen_len(&self) -> usize {
        self.seen_order.len()
    }

    fn matches_self_nickname(&self, sender: &str) -> bool {
        let sender_trimmed = sender.trim();
        self.self_nicknames
            .iter()
            .any(|nick| nick.eq_ignore_ascii_case(sender_trimmed) || nick == sender_trimmed)
    }

    fn peek_outbound_for_session(&self, session_key: &str, content: &str) -> bool {
        let norm_content = normalize_message_text(content);
        !norm_content.is_empty()
            && self.outbound_ring.iter().any(|entry| {
                session_keys_compatible(&entry.session_key, session_key)
                    && entry.normalized == norm_content
            })
    }

    fn consume_first_outbound_for_session(&mut self, session_key: &str, content: &str) -> bool {
        let norm_content = normalize_message_text(content);
        if norm_content.is_empty() {
            return false;
        }
        let pos = if !session_key.is_empty() {
            self.outbound_ring
                .iter()
                .position(|entry| {
                    entry.session_key == session_key && entry.normalized == norm_content
                })
                .or_else(|| {
                    self.outbound_ring.iter().position(|entry| {
                        session_keys_compatible(&entry.session_key, session_key)
                            && entry.normalized == norm_content
                    })
                })
        } else {
            self.outbound_ring
                .iter()
                .position(|entry| entry.normalized == norm_content)
        };
        if let Some(pos) = pos {
            self.outbound_ring.remove(pos);
            true
        } else {
            false
        }
    }

    fn peek_pending_stub_for_session(&self, session_key: &str, content: &str) -> bool {
        let norm_content = normalize_message_text(content);
        !norm_content.is_empty()
            && self.pending_stub_bindings.iter().any(|(entry, _)| {
                session_keys_compatible(&entry.session_key, session_key)
                    && entry.norm_text == norm_content
            })
    }

    fn insert_uid_alias(
        &mut self,
        ui_uid: String,
        onebot_message_id: i64,
        stub_uid: String,
        session_key: String,
    ) {
        let superseded_uids: HashSet<String> = self
            .uid_aliases
            .iter()
            .filter_map(|(existing_uid, (id, _, _))| {
                if *id == onebot_message_id && existing_uid != &ui_uid {
                    Some(existing_uid.clone())
                } else {
                    None
                }
            })
            .collect();
        if !superseded_uids.is_empty() {
            // 当同一条自发消息因状态跃迁产生新的 UI UID 时，淘汰旧瞬态 UID 防止后续同名文件碰撞
            self.uid_aliases.retain(|k, _| !superseded_uids.contains(k));
            self.uid_alias_order
                .retain(|k| !superseded_uids.contains(k));
            self.loose_uid_aliases
                .retain(|_, v| !superseded_uids.contains(v));
            self.loose_uid_order
                .retain(|k| self.loose_uid_aliases.contains_key(k));
            self.seen_ids.retain(|k| !superseded_uids.contains(k));
            self.seen_order.retain(|k| !superseded_uids.contains(k));
            self.confirmed_self_ids
                .retain(|k| !superseded_uids.contains(k));
        }
        if let Some(existing) = self.uid_aliases.get_mut(&ui_uid) {
            *existing = (onebot_message_id, stub_uid, session_key);
            return;
        }
        while self.uid_alias_order.len() >= MESSAGE_TRACKER_MAX_SEEN_IDS {
            if let Some(oldest) = self.uid_alias_order.pop_front() {
                self.uid_aliases.remove(&oldest);
            }
        }
        self.uid_alias_order.push_back(ui_uid.clone());
        self.uid_aliases
            .insert(ui_uid, (onebot_message_id, stub_uid, session_key));
    }

    /// 按 `(聊天类型:规范会话名, 发送者, 规范正文, 批次序号)` 锚定首次观察到的稳定 UID，防止 QQNT 补充或隐藏时间分隔导致消息重复上报
    fn resolve_or_register_loose_uid(
        &mut self,
        msg: &ChatMessage,
        occ_counts: &mut HashMap<(String, String, String), usize>,
    ) -> String {
        let base_key = (
            format!(
                "{}:{}",
                msg.target_kind.as_str(),
                canonical_group_name(&msg.group_name)
            ),
            msg.sender.trim().to_string(),
            normalize_message_text(&msg.content),
        );
        let occ = occ_counts.entry(base_key.clone()).or_insert(0);
        let current_occ = *occ;
        *occ += 1;

        if base_key.2.is_empty() {
            return msg.message_id.clone();
        }
        let full_key = (base_key.0, base_key.1, base_key.2, current_occ);
        if let Some(existing_uid) = self.loose_uid_aliases.get(&full_key) {
            return existing_uid.clone();
        }
        while self.loose_uid_order.len() >= MESSAGE_TRACKER_MAX_SEEN_IDS {
            if let Some(oldest) = self.loose_uid_order.pop_front() {
                self.loose_uid_aliases.remove(&oldest);
            }
        }
        self.loose_uid_order.push_back(full_key.clone());
        self.loose_uid_aliases
            .insert(full_key, msg.message_id.clone());
        msg.message_id.clone()
    }

    /// 改写同一轮快照内各消息的 UID 为首次观测到的规范 UID，消除时间戳动态渲染导致的 SHA-256 抖动
    fn stabilize_batch_uids(&mut self, messages: &mut [ChatMessage]) {
        let mut occ_counts: HashMap<(String, String, String), usize> =
            HashMap::with_capacity(messages.len());
        for msg in messages {
            let stable_uid = self.resolve_or_register_loose_uid(msg, &mut occ_counts);
            msg.message_id = stable_uid;
        }
    }

    fn prune_outbound(&mut self) {
        let now = Instant::now();
        while let Some(front) = self.outbound_ring.front() {
            if now.duration_since(front.sent_at) > OUTBOUND_TRACKER_TTL {
                self.outbound_ring.pop_front();
            } else {
                break;
            }
        }
        while let Some(front) = self.pending_stub_bindings.front() {
            if now.duration_since(front.1) > OUTBOUND_TRACKER_TTL {
                self.pending_stub_bindings.pop_front();
            } else {
                break;
            }
        }
        while let Some(front) = self.recent_self_media_tokens.front() {
            if now.duration_since(front.3) > OUTBOUND_TRACKER_TTL {
                self.recent_self_media_tokens.pop_front();
            } else {
                break;
            }
        }
    }

    fn insert_seen_id(&mut self, id: String) -> bool {
        if self.seen_ids.contains(&id) {
            return false;
        }
        while self.seen_order.len() >= MESSAGE_TRACKER_MAX_SEEN_IDS {
            if let Some(oldest) = self.seen_order.pop_front() {
                self.seen_ids.remove(&oldest);
                self.confirmed_self_ids.remove(&oldest);
            }
        }
        self.seen_ids.insert(id.clone());
        self.seen_order.push_back(id);
        true
    }

    /// 打开或切换会话时植入当前存量系统通知基准 ID，避免历史提示触发实时事件
    pub fn seed_notice_baseline(&mut self, notices: &[ParsedSystemNotice]) {
        for notice in notices {
            self.insert_seen_notice_id(notice.notice_id.clone());
        }
    }

    /// 过滤当前可见系统提示列表并返回本轮首次出现的新增通知集合
    pub fn observe_notices(&mut self, notices: Vec<ParsedSystemNotice>) -> Vec<ParsedSystemNotice> {
        notices
            .into_iter()
            .filter(|notice| self.insert_seen_notice_id(notice.notice_id.clone()))
            .collect()
    }

    fn insert_seen_notice_id(&mut self, id: String) -> bool {
        if self.seen_notice_ids.contains(&id) {
            return false;
        }
        while self.seen_notice_order.len() >= MESSAGE_TRACKER_MAX_SEEN_IDS {
            if let Some(oldest) = self.seen_notice_order.pop_front() {
                self.seen_notice_ids.remove(&oldest);
            }
        }
        self.seen_notice_ids.insert(id.clone());
        self.seen_notice_order.push_back(id);
        true
    }
}

fn is_ignored_media_uri_prefix(trimmed: &str) -> bool {
    ["base64://", "data:", "http://", "https://"]
        .iter()
        .any(|prefix| {
            trimmed
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        })
}

pub(super) fn normalize_media_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || is_ignored_media_uri_prefix(trimmed) {
        return None;
    }
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).trim();
    let norm = normalize_message_text(base).to_ascii_lowercase();
    if norm.is_empty() {
        None
    } else {
        Some(norm)
    }
}

fn extract_polled_media_token(msg: &ChatMessage) -> Option<String> {
    for seg in &msg.segments {
        match seg {
            MessageSegment::File { file, name, .. } => {
                if let Some(tok) = name
                    .as_deref()
                    .and_then(normalize_media_token)
                    .or_else(|| normalize_media_token(file))
                {
                    return Some(tok);
                }
            }
            MessageSegment::Video { file, .. } => {
                if let Some(tok) = normalize_media_token(file) {
                    return Some(tok);
                }
            }
            _ => {}
        }
    }

    let normalized = msg
        .content
        .replace(['\u{2009}', '\u{00A0}', '\u{202F}', '\u{3000}'], " ");
    let has_size = FILE_BUBBLE_SIZE_REGEX.is_match(&normalized);
    let has_percent = normalized.split_whitespace().any(|tok| {
        tok.strip_suffix('%')
            .and_then(|digits| digits.parse::<u16>().ok())
            .is_some_and(|pct| pct <= 100)
    });
    let has_status_or_size = has_size
        || FILE_BUBBLE_STATUS_KEYWORDS
            .iter()
            .any(|kw| normalized.contains(kw))
        || (has_size && has_percent);
    if !has_status_or_size {
        return None;
    }
    let first_line = normalized
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && *line != IMAGE_PLACEHOLDER_TOKEN)?;
    normalize_media_token(first_line)
}
