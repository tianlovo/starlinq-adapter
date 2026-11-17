//! 模拟窗口共享状态、Win32 实例控件句柄及异步请求载体定义

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};

use windows::Win32::Foundation::HWND;

use crate::constants::{
    INITIAL_MOCK_GROUPS, MENTION_ALL_TOKEN, MENTION_ME_QQ_BOT, MENTION_ME_SANDRONE,
    PREVIEW_SNIPPET_MAX_CHARS, SIDEBAR_AT_ALL_TAG, SIDEBAR_AT_ME_TAG, SIDEBAR_MOCK_TIME,
};

fn message_mentions_me(msg: &str) -> bool {
    msg.contains(MENTION_ME_SANDRONE) || msg.to_ascii_lowercase().contains(MENTION_ME_QQ_BOT)
}

fn sanitize_preview_snippet(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let snippet: String = chars.by_ref().take(PREVIEW_SNIPPET_MAX_CHARS).collect();
    snippet
}

/// 跨线程共享的模拟 QQ 会话与聊天历史状态
#[derive(Debug)]
pub(crate) struct SharedMockState {
    pub groups: Vec<String>,
    pub active_group: String,
    pub messages: HashMap<String, Vec<String>>,
    pub unread_counts: HashMap<String, u32>,
    pub last_preview: HashMap<String, String>,
}

impl SharedMockState {
    /// 使用预设大群与测试小群记录构造初始模拟状态
    pub(crate) fn new() -> Self {
        let initial_groups = INITIAL_MOCK_GROUPS
            .iter()
            .map(|(group_title, initial_line)| {
                (
                    (*group_title).to_string(),
                    vec![(*initial_line).to_string()],
                )
            })
            .collect();
        Self::from_groups(initial_groups)
    }

    /// 使用自定义会话列表与各自初始消息记录构造模拟状态
    pub(crate) fn from_groups(initial_groups: Vec<(String, Vec<String>)>) -> Self {
        let mut groups = Vec::with_capacity(initial_groups.len());
        let mut messages: HashMap<String, Vec<String>> =
            HashMap::with_capacity(initial_groups.len());

        for (group_title, initial_lines) in initial_groups {
            if let Some(existing) = messages.get_mut(&group_title) {
                existing.extend(initial_lines);
            } else {
                groups.push(group_title.clone());
                messages.insert(group_title, initial_lines);
            }
        }

        let active_group = groups.first().cloned().unwrap_or_default();
        Self {
            groups,
            active_group,
            messages,
            unread_counts: HashMap::new(),
            last_preview: HashMap::new(),
        }
    }

    /// 追加批量入站消息；若目标会话非当前激活会话则累加未读计数并生成带 `@` 标记的单行预览
    pub(crate) fn record_incoming_batch(&mut self, group_name: &str, formatted_msgs: &[String]) {
        if !self.groups.iter().any(|existing| existing == group_name) {
            self.groups.push(group_name.to_string());
        }
        self.messages
            .entry(group_name.to_string())
            .or_default()
            .extend(formatted_msgs.iter().cloned());

        if formatted_msgs.is_empty() || group_name == self.active_group {
            return;
        }

        let prev_unread = self.unread_counts.get(group_name).copied().unwrap_or(0);
        let next_unread = prev_unread.saturating_add(formatted_msgs.len() as u32);
        self.unread_counts
            .insert(group_name.to_string(), next_unread);

        let batch_has_at_me = formatted_msgs.iter().any(|msg| message_mentions_me(msg));
        let batch_has_at_all = formatted_msgs
            .iter()
            .any(|msg| msg.contains(MENTION_ALL_TOKEN));
        let prev_preview = self
            .last_preview
            .get(group_name)
            .map(String::as_str)
            .unwrap_or("");
        let prev_has_at_me = prev_unread > 0 && prev_preview.starts_with(SIDEBAR_AT_ME_TAG);
        let prev_has_at_all = prev_unread > 0 && prev_preview.starts_with(SIDEBAR_AT_ALL_TAG);

        let tag = if batch_has_at_me || prev_has_at_me {
            SIDEBAR_AT_ME_TAG
        } else if batch_has_at_all || prev_has_at_all {
            SIDEBAR_AT_ALL_TAG
        } else {
            ""
        };

        let latest_snippet = formatted_msgs
            .last()
            .map(|msg| sanitize_preview_snippet(msg))
            .unwrap_or_default();
        let combined_preview = if latest_snippet.is_empty() {
            tag.trim_end().to_string()
        } else {
            format!("{tag}{latest_snippet}")
        };
        if !combined_preview.is_empty() {
            self.last_preview
                .insert(group_name.to_string(), combined_preview);
        }
    }

    /// 清除指定会话的未读消息计数与提醒预览，返回此前是否存在未读或提醒状态
    pub(crate) fn clear_unread(&mut self, group_name: &str) -> bool {
        let had_unread = self.unread_counts.get(group_name).copied().unwrap_or(0) > 0;
        let had_preview = self.last_preview.remove(group_name).is_some();
        self.unread_counts.insert(group_name.to_string(), 0);
        had_unread || had_preview
    }

    /// 将规范群名与当前未读计数、单行预览格式化为可被 `session_matcher` 解析的侧边栏行文本
    pub(crate) fn format_session_row(&self, group_title: &str) -> String {
        let unread = self.unread_counts.get(group_title).copied().unwrap_or(0);
        let preview = self
            .last_preview
            .get(group_title)
            .map(|text| text.trim())
            .filter(|text| !text.is_empty());

        match (unread > 0, preview) {
            (true, Some(snippet)) => {
                format!("{group_title} {SIDEBAR_MOCK_TIME} {snippet} ({unread}条未读)")
            }
            (true, None) => format!("{group_title} {SIDEBAR_MOCK_TIME} ({unread}条未读)"),
            (false, Some(snippet)) => format!("{group_title} {SIDEBAR_MOCK_TIME} {snippet}"),
            (false, None) => group_title.to_string(),
        }
    }
}

/// 绑定在模拟主窗口 `GWLP_USERDATA` 上的 Win32 子控件句柄集合
pub(crate) struct WindowInstanceState {
    pub shared: Arc<Mutex<SharedMockState>>,
    pub list_sessions: HWND,
    pub static_header: HWND,
    pub list_history: HWND,
    pub edit_composer: HWND,
    pub btn_send: HWND,
    pub security_hwnd: HWND,
    pub suppress_selection_sync: Cell<bool>,
}

/// 跨线程投递给模拟窗口消息泵的带确认回执请求（Command 载荷）
pub(crate) struct MockAsyncRequest {
    pub security_title: Option<String>,
    pub ack_sender: mpsc::SyncSender<()>,
}
