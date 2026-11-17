//! 嵌入式仓储门面（Facade + Repository + Parameter Object）：管理 SQLite 连接与各领域仓储接口

pub mod constants;
pub mod models;
pub mod repositories;
mod schema;

#[cfg(test)]
mod remediation_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod wave6_tests;

use std::path::Path;

use common_types::{
    AdapterError, AuditEventRecord, ChatTargetKind, FriendInfo, GroupInfoSummary, GroupMemberInfo,
    MessageHistoryFilter, Result, StorageStats, StoredMessageRecord,
};
use parking_lot::Mutex;
use rusqlite::Connection;
use serde::Serialize;
use tracing::debug;

use crate::constants::MEMORY_DB_PATH;
use crate::repositories::{
    friend_repo, group_repo, history_query_repo, map_sqlite_err, member_repo, message_repo,
    meta_audit_repo,
};

pub use models::*;

/// 线程安全的嵌入式 SQLite 持久化门面，封装 WAL 连接调优及各仓储操作
#[derive(Debug)]
pub struct Storage {
    conn: Mutex<Connection>,
    db_path: String,
}

impl Storage {
    /// 打开或创建指定路径（或 `:memory:`）的 SQLite 数据库，应用内存调优并执行幂等建表迁移
    pub fn open(db_path: &str) -> Result<Self> {
        let trimmed_path = db_path.trim();
        if trimmed_path.is_empty() {
            return Err(AdapterError::ConfigError(
                "storage db_path must not be empty".to_string(),
            ));
        }

        let is_memory = trimmed_path == MEMORY_DB_PATH;
        let conn = if is_memory {
            Connection::open_in_memory().map_err(map_sqlite_err)?
        } else {
            let file_path = Path::new(trimmed_path);
            if let Some(parent_dir) = file_path.parent() {
                if !parent_dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent_dir)?;
                }
            }
            Connection::open(file_path).map_err(map_sqlite_err)?
        };

        schema::apply_pragmas(&conn, is_memory)?;
        schema::initialize_schema(&conn)?;
        debug!(
            db_path = trimmed_path,
            is_memory, "嵌入式 SQLite 存储初始化完成"
        );

        Ok(Self {
            conn: Mutex::new(conn),
            db_path: trimmed_path.to_string(),
        })
    }

    /// 返回当前数据库路径配置字符串（`:memory:` 或文件系统路径）
    pub fn db_path(&self) -> &str {
        &self.db_path
    }

    /// 判定当前存储是否处于隔离的 `:memory:` 内存数据库模式
    pub fn is_memory(&self) -> bool {
        self.db_path == MEMORY_DB_PATH
    }

    /// 插入或更新群聊元数据，传入字段为 `None` 时保留库内已有真实群号与成员统计，并在发现真实群号时同步修正历史消息
    pub fn upsert_group(&self, input: &UpsertGroupInput) -> Result<StoredGroupRecord> {
        let mut conn = self.conn.lock();
        group_repo::upsert_group(&mut conn, input)
    }

    /// 在单事务内批量更新多条群聊摘要元数据
    pub fn upsert_groups_batch(&self, inputs: &[UpsertGroupInput]) -> Result<()> {
        let mut conn = self.conn.lock();
        group_repo::upsert_groups_batch(&mut conn, inputs)
    }

    /// 更新指定群聊右侧抽屉最近一次完成成员巡检的时间戳
    pub fn mark_group_drawer_checked(&self, group_name: &str, checked_at_ms: u64) -> Result<()> {
        let conn = self.conn.lock();
        group_repo::mark_group_drawer_checked(&conn, group_name, checked_at_ms)
    }

    /// 按群显示名称精确查询群元数据记录
    pub fn get_group_by_name(&self, group_name: &str) -> Result<Option<StoredGroupRecord>> {
        let conn = self.conn.lock();
        group_repo::get_group_by_name(&conn, group_name)
    }

    /// 按真实群号或确定性 OneBot 群号解析对应群元数据记录
    pub fn get_group_by_id(&self, group_id: i64) -> Result<Option<StoredGroupRecord>> {
        let conn = self.conn.lock();
        group_repo::get_group_by_id(&conn, group_id)
    }

    /// 按最近更新时间倒序列出全部已缓存群摘要
    pub fn list_groups(&self) -> Result<Vec<GroupInfoSummary>> {
        let conn = self.conn.lock();
        group_repo::list_groups(&conn)
    }

    /// 清理尚未绑定真实群号且成员数为 0 的临时或单行拼接污染群记录
    pub fn delete_transient_group_by_name(&self, group_name: &str) -> Result<bool> {
        let conn = self.conn.lock();
        group_repo::delete_transient_group_by_name(&conn, group_name)
    }

    /// 清理无真实群号、未通过标题校验且无消息或成员引用的群聊污染记录，并写入审计日志
    pub fn purge_invalid_groups<F>(&self, is_valid_title: F) -> Result<Vec<String>>
    where
        F: Fn(&str) -> bool,
    {
        let mut conn = self.conn.lock();
        group_repo::purge_invalid_groups(&mut conn, is_valid_title)
    }

    /// 插入或合并更新单个好友联系人元数据
    pub fn upsert_friend(&self, input: &UpsertFriendInput) -> Result<StoredFriendRecord> {
        let mut conn = self.conn.lock();
        friend_repo::upsert_friend(&mut conn, input)
    }

    /// 在单事务内批量更新多条好友联系人摘要元数据
    pub fn upsert_friends_batch(&self, inputs: &[UpsertFriendInput]) -> Result<()> {
        let mut conn = self.conn.lock();
        friend_repo::upsert_friends_batch(&mut conn, inputs)
    }

    /// 按最近更新时间倒序列出全部已缓存好友概要信息
    pub fn list_friends(&self) -> Result<Vec<FriendInfo>> {
        let conn = self.conn.lock();
        friend_repo::list_friends(&conn)
    }

    /// 按真实 QQ 号或合成 OneBot 用户编号检索单个好友记录
    pub fn get_friend_by_id(&self, user_id: i64) -> Result<Option<StoredFriendRecord>> {
        let conn = self.conn.lock();
        friend_repo::get_friend_by_id(&conn, user_id)
    }

    /// 按好友昵称或备注精确查询好友持久化记录
    pub fn get_friend_by_name(&self, name: &str) -> Result<Option<StoredFriendRecord>> {
        let conn = self.conn.lock();
        friend_repo::get_friend_by_name(&conn, name)
    }

    /// 按主键、昵称或备注删除误写入 `friends` 表中的好友污染记录（存在且删除时返回 `true`）
    pub fn delete_friend_by_name(&self, name: &str) -> Result<bool> {
        let conn = self.conn.lock();
        friend_repo::delete_friend_by_name(&conn, name)
    }

    /// 清理无真实 QQ 号、未通过标题校验且无私聊消息引用的好友污染记录，并写入审计日志
    pub fn purge_invalid_friends<F>(&self, is_valid_title: F) -> Result<Vec<String>>
    where
        F: Fn(&str) -> bool,
    {
        let mut conn = self.conn.lock();
        friend_repo::purge_invalid_friends(&mut conn, is_valid_title)
    }

    /// 在单个数据库事务内批量写入或合并指定群聊的成员列表
    pub fn upsert_group_members(
        &self,
        group_name: &str,
        group_id: i64,
        members: &[UpsertMemberInput],
    ) -> Result<()> {
        let mut conn = self.conn.lock();
        member_repo::upsert_group_members(&mut conn, group_name, group_id, members)
    }

    /// 跨命名空间统一群成员及对应历史群消息的映射用户 ID，返回实际更新的记录条数
    pub fn reconcile_member_mapped_ids(
        &self,
        self_nickname: Option<&str>,
        self_user_id: Option<i64>,
    ) -> Result<usize> {
        let mut conn = self.conn.lock();
        member_repo::reconcile_member_mapped_ids(&mut conn, self_nickname, self_user_id)
    }

    /// 列出指定群聊的全部缓存成员，并统一附带调用方传入的有效群号
    pub fn list_group_members(
        &self,
        group_name: &str,
        effective_group_id: i64,
    ) -> Result<Vec<GroupMemberInfo>> {
        let conn = self.conn.lock();
        member_repo::list_group_members(&conn, group_name, effective_group_id)
    }

    /// 按真实 QQ 号或合成 OneBot 用户编号检索单个群成员信息
    pub fn get_group_member_by_id(
        &self,
        group_name: &str,
        user_id: i64,
        effective_group_id: i64,
    ) -> Result<Option<GroupMemberInfo>> {
        let conn = self.conn.lock();
        member_repo::get_group_member_by_id(&conn, group_name, user_id, effective_group_id)
    }

    /// 幂等持久化群消息并返回数字 OneBot 消息编号（重复 `message_uid` 不重复递增序列）
    pub fn record_message(&self, input: &RecordMessageInput) -> Result<i64> {
        let mut conn = self.conn.lock();
        message_repo::record_message(&mut conn, input)
    }

    /// 为已确认发送或转发的出站消息强制写入全新行并返回 `(onebot_message_id, stored_message_uid)`
    pub fn record_confirmed_outbound_message(
        &self,
        input: &RecordMessageInput,
    ) -> Result<(i64, String)> {
        let mut conn = self.conn.lock();
        message_repo::record_confirmed_outbound_message(&mut conn, input)
    }

    /// 在单事务内批量幂等持久化群聊消息并按原顺序返回各自的一一对应数字编号
    pub fn record_messages_batch(&self, inputs: &[RecordMessageInput]) -> Result<Vec<i64>> {
        let mut conn = self.conn.lock();
        message_repo::record_messages_batch(&mut conn, inputs)
    }

    /// 将出站存根消息的 `message_uid` 重绑定为 UI 回采 UID，并清理较新的重复回采行以维持单气泡单行
    pub fn reconcile_outbound_stub_uid(
        &self,
        onebot_message_id: i64,
        ui_uid: &str,
        timestamp_hint: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.conn.lock();
        message_repo::reconcile_outbound_stub_uid(
            &mut conn,
            onebot_message_id,
            ui_uid,
            timestamp_hint,
        )
    }

    /// 按数字 OneBot 消息编号查询单条已存储聊天消息
    pub fn get_message_by_id(&self, onebot_message_id: i64) -> Result<Option<StoredMessageRecord>> {
        let conn = self.conn.lock();
        message_repo::get_message_by_id(&conn, onebot_message_id)
    }

    /// 按数字 OneBot 消息编号删除已撤回的消息记录并返回是否确有删除行
    pub fn delete_message_by_id(&self, onebot_message_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        message_repo::delete_message_by_id(&conn, onebot_message_id)
    }

    /// 按 OneBot 消息编号删除已撤回的消息记录并返回是否确有删除行
    pub fn delete_message_by_onebot_id(&self, onebot_message_id: i64) -> Result<bool> {
        self.delete_message_by_id(onebot_message_id)
    }

    /// 优先按消失气泡 UID 列表（偏好匹配发送者与 `actor_is_self`），其次按 180 秒时间窗解析被撤回的聊天消息
    pub fn resolve_recalled_message(
        &self,
        group_name: &str,
        disappeared_uids: &[String],
        sender_name: Option<&str>,
        actor_is_self: bool,
    ) -> Result<Option<StoredMessageRecord>> {
        let conn = self.conn.lock();
        message_repo::resolve_recalled_message(
            &conn,
            group_name,
            disappeared_uids,
            sender_name,
            actor_is_self,
        )
    }

    /// 在指定会话类型与会话名称内优先按消失气泡 UID、其次按 180 秒时间窗解析被撤回的消息记录
    pub fn resolve_recalled_message_in_session(
        &self,
        group_name: &str,
        chat_type: ChatTargetKind,
        disappeared_uids: &[String],
        sender_name: Option<&str>,
        actor_is_self: bool,
    ) -> Result<Option<StoredMessageRecord>> {
        let conn = self.conn.lock();
        message_repo::resolve_recalled_message_with_kind(
            &conn,
            (group_name, Some(chat_type)),
            disappeared_uids,
            sender_name,
            actor_is_self,
        )
    }

    /// 按从旧到新时间顺序返回指定群或私聊最近 `limit` 条缓存消息
    pub fn list_recent_messages(
        &self,
        group_name: &str,
        limit: usize,
    ) -> Result<Vec<StoredMessageRecord>> {
        let conn = self.conn.lock();
        message_repo::list_recent_messages(&conn, group_name, limit)
    }

    /// 按可选会话类型（群聊或私聊）过滤并返回指定会话最近 `limit` 条缓存消息（按时间从旧到新排列）
    pub fn list_recent_messages_by_kind(
        &self,
        group_name: &str,
        chat_type: Option<ChatTargetKind>,
        limit: usize,
    ) -> Result<Vec<StoredMessageRecord>> {
        let conn = self.conn.lock();
        message_repo::list_recent_messages_by_kind(&conn, group_name, chat_type, limit)
    }

    /// 按会话名、会话类型、游标范围、发送者与关键词深度分页查询离线历史消息
    pub fn query_messages_filtered(
        &self,
        filter: &MessageHistoryFilter,
    ) -> Result<Vec<StoredMessageRecord>> {
        let conn = self.conn.lock();
        history_query_repo::query_messages_filtered(&conn, filter)
    }

    /// 在指定会话近期消息历史中按发送者线索与文本摘要匹配被引用消息的 OneBot 编号
    pub fn find_recent_message_for_reply(
        &self,
        group_name: &str,
        sender_hint: Option<&str>,
        content_excerpt: &str,
    ) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        message_repo::find_recent_message_for_reply(&conn, group_name, sender_hint, content_excerpt)
    }

    /// 在指定会话类型与会话名称的近期消息历史中按发送者线索与文本摘要匹配被引用消息的 OneBot 编号
    pub fn find_recent_message_for_reply_in_session(
        &self,
        group_name: &str,
        chat_type: ChatTargetKind,
        sender_hint: Option<&str>,
        content_excerpt: &str,
    ) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        message_repo::find_recent_message_for_reply_with_kind(
            &conn,
            (group_name, Some(chat_type)),
            sender_hint,
            content_excerpt,
        )
    }

    /// 原子分配全局单调递增的 `fwd-*` 编号并安全写入合并转发节点内容
    pub fn allocate_forward_nodes(&self, nodes_json: &str) -> Result<String> {
        let mut conn = self.conn.lock();
        meta_audit_repo::allocate_forward_nodes(&mut conn, nodes_json)
    }

    /// 按入站消息 UID 幂等复用或原子分配合并转发编号，并对其节点包与 UID 索引维持有界容量
    pub fn get_or_allocate_forward_for_uid(
        &self,
        message_uid: &str,
        nodes_json: &str,
    ) -> Result<String> {
        let mut conn = self.conn.lock();
        meta_audit_repo::get_or_allocate_forward_for_uid(&mut conn, message_uid, nodes_json)
    }

    /// 写入或覆盖键值对运行元数据
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock();
        meta_audit_repo::set_meta(&conn, key, value)
    }

    /// 按键名读取运行元数据
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        meta_audit_repo::get_meta(&conn, key)
    }

    /// 追加一条原始 JSON 字符串格式的审计事件记录
    pub fn append_audit(&self, event_kind: &str, detail_json: &str) -> Result<()> {
        let conn = self.conn.lock();
        meta_audit_repo::append_audit(&conn, event_kind, detail_json)
    }

    /// 序列化任意结构体为 JSON 并追加审计事件记录
    pub fn append_audit_json<T: Serialize>(&self, event_kind: &str, detail: &T) -> Result<()> {
        let conn = self.conn.lock();
        meta_audit_repo::append_audit_json(&conn, event_kind, detail)
    }

    /// 按可选事件类型过滤并按主键倒序分页查询持久化的审计事件记录
    pub fn list_audit_events(
        &self,
        event_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AuditEventRecord>> {
        let conn = self.conn.lock();
        meta_audit_repo::list_audit_events(&conn, event_kind, limit)
    }

    /// 汇总数据库各业务表当前行数统计信息
    pub fn stats(&self) -> Result<StorageStats> {
        let conn = self.conn.lock();
        meta_audit_repo::stats(&conn, &self.db_path)
    }

    #[cfg(test)]
    pub(crate) fn inspect_pragma_text(&self, pragma_sql: &str) -> String {
        let conn = self.conn.lock();
        conn.query_row(pragma_sql, [], |row| row.get::<_, String>(0))
            .expect("query text pragma")
    }

    #[cfg(test)]
    pub(crate) fn inspect_pragma_i64(&self, pragma_sql: &str) -> i64 {
        let conn = self.conn.lock();
        conn.query_row(pragma_sql, [], |row| row.get::<_, i64>(0))
            .expect("query integer pragma")
    }
}
