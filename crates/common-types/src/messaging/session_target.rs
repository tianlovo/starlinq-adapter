//! 左侧会话条目摘要与目标群/私聊安全守卫规格定义

use serde::{Deserialize, Serialize};

use crate::constants::{DEFAULT_STRICT_BELOW_MEMBERS, DEFAULT_TARGET_GROUP_NAME};
use crate::onebot::canonical_group_names_match;

/// 从 QQ 左侧会话列表提取的会话项摘要
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionSummary {
    pub title: String,
    pub raw_name: String,
    pub preview: Option<String>,
    pub unread_count: Option<u32>,
    pub member_count_hint: Option<u32>,
    pub rect: [i32; 4],
    #[serde(default)]
    pub is_group_hint: Option<bool>,
    #[serde(default)]
    pub has_at_me_hint: bool,
    #[serde(default)]
    pub has_at_all_hint: bool,
}

/// 打开并校验目标群聊与私聊白名单时使用的安全守卫规格
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupTargetSpec {
    pub group_name: String,
    pub strict_below_members: Option<u32>,
    pub require_member_count_verified: bool,
    #[serde(default)]
    pub allowed_private_friends: Vec<String>,
    #[serde(default)]
    pub monitored_groups: Vec<String>,
    #[serde(default)]
    pub allow_large_monitored_groups: bool,
}

fn matches_named_target(entry: &str, needle: &str) -> bool {
    let trimmed = entry.trim();
    !trimmed.is_empty() && (trimmed == needle || canonical_group_names_match(trimmed, needle))
}

impl GroupTargetSpec {
    /// 校验目标好友昵称或备注是否显式存在于私聊白名单中（空白名单一律返回 `false`）
    pub fn is_private_friend_allowed(&self, candidate: &str) -> bool {
        let needle = candidate.trim();
        if needle.is_empty() {
            return false;
        }
        self.allowed_private_friends
            .iter()
            .any(|entry| matches_named_target(entry, needle))
    }

    /// 返回生效的目标监控群列表（优先使用去重后的 `monitored_groups`，为空时回退至 `group_name`）
    pub fn effective_monitored_groups(&self) -> Vec<String> {
        let mut groups = Vec::new();
        for raw in &self.monitored_groups {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !groups
                .iter()
                .any(|existing: &String| matches_named_target(existing, trimmed))
            {
                groups.push(trimmed.to_string());
            }
        }
        if groups.is_empty() {
            let fallback = self.group_name.trim();
            if !fallback.is_empty() {
                groups.push(fallback.to_string());
            }
        }
        groups
    }

    /// 判断候选群名是否命中当前生效的目标监控群列表（支持剥离人数后缀后比对）
    pub fn is_monitored_group(&self, candidate: &str) -> bool {
        let needle = candidate.trim();
        if needle.is_empty() {
            return false;
        }
        self.effective_monitored_groups()
            .iter()
            .any(|entry| matches_named_target(entry, needle))
    }

    /// 为指定候选群构造安全守卫（仅在显式开启 `allow_large_monitored_groups` 且命中监控列表时放开人数上限）
    pub fn member_guard_for_group(&self, candidate: &str) -> GroupTargetSpec {
        let mut guard = self.clone();
        let trimmed = candidate.trim();
        if !trimmed.is_empty() {
            guard.group_name = trimmed.to_string();
        }
        if self.allow_large_monitored_groups && self.is_monitored_group(trimmed) {
            guard.strict_below_members = None;
        }
        guard
    }
}

impl Default for GroupTargetSpec {
    fn default() -> Self {
        Self {
            group_name: DEFAULT_TARGET_GROUP_NAME.to_string(),
            strict_below_members: Some(DEFAULT_STRICT_BELOW_MEMBERS),
            require_member_count_verified: false,
            allowed_private_friends: Vec::new(),
            monitored_groups: Vec::new(),
            allow_large_monitored_groups: false,
        }
    }
}
