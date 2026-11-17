//! 嵌入式 SQLite 存储状态查看子命令模块：输出各核心表统计与已缓存群概要

use anyhow::Result;
use common_types::{
    AdapterConfig, BotDriver, FriendInfo, GroupInfoSummary, GroupMemberInfo, StorageStats,
};
use reqwest::Method;
use serde::Deserialize;

use crate::commands::dispatch_remote_or_local;
use crate::constants::{
    ROUTE_API_FRIENDS, ROUTE_API_GROUPS, ROUTE_API_GROUP_INFO, ROUTE_API_GROUP_MEMBERS,
    ROUTE_API_STORAGE,
};
use crate::http_client::RemoteClient;

/// `/api/storage` 返回的数据库统计、已缓存群与好友集合反序列化载体
#[derive(Debug, Deserialize)]
struct StorageOverviewDto {
    stats: StorageStats,
    groups: Vec<GroupInfoSummary>,
    #[serde(default)]
    friends: Vec<FriendInfo>,
}

fn print_friends_list(friends: &[FriendInfo]) {
    println!("好友列表 ({}):", friends.len());
    for friend in friends {
        println!(
            "  user_id={} real_id={:?} nickname='{}' remark='{}'",
            friend.user_id, friend.real_user_id, friend.nickname, friend.remark
        );
    }
}

fn print_groups_list(groups: &[GroupInfoSummary]) {
    println!("群列表 ({}):", groups.len());
    for group in groups {
        println!(
            "  group_id={} real_id={:?} name='{}' members={}/{}",
            group.group_id,
            group.real_group_id,
            group.group_name,
            group.member_count,
            group.max_member_count
        );
    }
}

fn print_group_members_list(members: &[GroupMemberInfo]) {
    println!("群成员列表 ({}):", members.len());
    for member in members {
        println!(
            "  user_id={} real_id={:?} role={} nickname='{}' card='{}'",
            member.user_id, member.real_user_id, member.role, member.nickname, member.card
        );
    }
}

fn print_storage_overview(
    stats: &StorageStats,
    groups: &[GroupInfoSummary],
    friends: &[FriendInfo],
) {
    println!("================ 嵌入式存储概览 ================");
    println!("数据库路径      : {}", stats.db_path);
    println!("已缓存群数量    : {}", stats.group_count);
    println!("已缓存好友数量  : {}", stats.friend_count);
    println!("已缓存成员数量  : {}", stats.member_count);
    println!("历史消息总数    : {}", stats.message_count);
    println!("发送审计日志数  : {}", stats.audit_count);
    print_groups_list(groups);
    print_friends_list(friends);
    println!("================================================");
}

/// 查询并输出 SQLite 持久化统计计数、群列表与好友列表缓存摘要
pub async fn cmd_storage(config: AdapterConfig, remote: Option<RemoteClient>) -> Result<()> {
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        |client| async move {
            let overview: StorageOverviewDto =
                client.call(Method::GET, ROUTE_API_STORAGE, None).await?;
            print_storage_overview(&overview.stats, &overview.groups, &overview.friends);
            Ok(())
        },
        |driver, _status| async move {
            let stats = driver.storage_stats().await?;
            let groups = driver.get_group_list(false).await?;
            let friends = driver.get_friend_list(false).await?;
            print_storage_overview(&stats, &groups, &friends);
            Ok(())
        },
    )
    .await
}

/// 查询并输出已缓存或实时刷新后的好友列表元数据
pub async fn cmd_friends(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    no_cache: bool,
) -> Result<()> {
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let flag = if no_cache { "true" } else { "false" };
            let friends: Vec<FriendInfo> = client
                .call_query(ROUTE_API_FRIENDS, &[("no_cache", flag)])
                .await?;
            print_friends_list(&friends);
            Ok(())
        },
        move |driver, _status| async move {
            let friends = driver.get_friend_list(no_cache).await?;
            print_friends_list(&friends);
            Ok(())
        },
    )
    .await
}

fn build_group_lookup_query(
    group: Option<String>,
    group_id: Option<i64>,
    no_cache: bool,
) -> Vec<(&'static str, String)> {
    let mut pairs = Vec::new();
    if let Some(name) = group
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
    {
        pairs.push(("group", name.to_string()));
    }
    if let Some(gid) = group_id {
        pairs.push(("group_id", gid.to_string()));
    }
    if no_cache {
        pairs.push(("no_cache", "true".to_string()));
    }
    pairs
}

/// 查询并输出已缓存或强刷后的群列表元数据
pub async fn cmd_groups(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    no_cache: bool,
) -> Result<()> {
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| async move {
            let flag = if no_cache { "true" } else { "false" };
            let groups: Vec<GroupInfoSummary> = client
                .call_query(ROUTE_API_GROUPS, &[("no_cache", flag)])
                .await?;
            print_groups_list(&groups);
            Ok(())
        },
        move |driver, _status| async move {
            let groups = driver.get_group_list(no_cache).await?;
            print_groups_list(&groups);
            Ok(())
        },
    )
    .await
}

/// 查询并输出目标群详情及真实数字群号探测结果
pub async fn cmd_group_info(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    group: Option<String>,
    group_id: Option<i64>,
    no_cache: bool,
) -> Result<()> {
    let group_for_remote = group.clone();
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| {
            let pairs = build_group_lookup_query(group_for_remote.clone(), group_id, no_cache);
            async move {
                let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let info: GroupInfoSummary = client.call_query(ROUTE_API_GROUP_INFO, &refs).await?;
                print_group_info_summary(&info);
                Ok(())
            }
        },
        move |driver, _status| {
            let group_local = group.clone();
            async move {
                let info = driver
                    .get_group_info(group_id, group_local, no_cache)
                    .await?;
                print_group_info_summary(&info);
                Ok(())
            }
        },
    )
    .await
}

/// 查询并输出目标群成员列表（支持缓存命中与 `--no-cache` 强刷）
pub async fn cmd_group_members(
    config: AdapterConfig,
    remote: Option<RemoteClient>,
    group: Option<String>,
    group_id: Option<i64>,
    no_cache: bool,
) -> Result<()> {
    let default_group = config.target.group_name.clone();
    let group_for_remote = group.clone();
    dispatch_remote_or_local(
        config,
        remote,
        ".\\start-daemon.bat",
        move |client| {
            let pairs = build_group_lookup_query(group_for_remote.clone(), group_id, no_cache);
            async move {
                let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let members: Vec<GroupMemberInfo> =
                    client.call_query(ROUTE_API_GROUP_MEMBERS, &refs).await?;
                print_group_members_list(&members);
                Ok(())
            }
        },
        move |driver, status| {
            let trimmed_group = group
                .as_deref()
                .map(str::trim)
                .filter(|val| !val.is_empty())
                .map(ToOwned::to_owned);
            let resolved_group = if group_id.is_none() && trimmed_group.is_none() {
                Some(status.active_group.unwrap_or(default_group))
            } else {
                trimmed_group
            };
            async move {
                let members = driver
                    .get_group_member_list(group_id, resolved_group, no_cache)
                    .await?;
                print_group_members_list(&members);
                Ok(())
            }
        },
    )
    .await
}

/// 格式化输出单群摘要与真实群号字段
fn print_group_info_summary(info: &GroupInfoSummary) {
    println!("================ 群信息摘要 ================");
    println!("Group Name      : {}", info.group_name);
    println!("OneBot Group ID : {}", info.group_id);
    println!("Real Group ID   : {:?}", info.real_group_id);
    println!(
        "Members         : {} / {}",
        info.member_count, info.max_member_count
    );
    println!("============================================");
}
