//! QQNT 右侧 `"群成员列表"` 侧边栏解析器，用于提取可见成员昵称及群主/管理员角色

use std::collections::HashSet;

use common_types::constants::{
    UI_LABEL_GROUP_MEMBER_LIST, UI_NOISE_LEVEL_PREFIX, UI_ROLE_ADMIN_LABEL, UI_ROLE_OWNER_LABEL,
};
use common_types::UiNodeSnapshot;
use storage::UpsertMemberInput;
use uia_core::constants::{
    CONTROL_TYPE_BUTTON, CONTROL_TYPE_EDIT, CONTROL_TYPE_GROUP, CONTROL_TYPE_LIST,
    CONTROL_TYPE_LIST_ITEM, CONTROL_TYPE_TEXT, CONTROL_TYPE_WINDOW,
};

use crate::constants::{LEVEL_BADGE_REGEX, MEMBER_SIDEBAR_COUNT_REGEX, SEARCH_KEYWORD};
use crate::profile;

fn is_member_list_container(node: &UiNodeSnapshot) -> bool {
    if node.is_offscreen {
        return false;
    }
    let control_type = node.control_type.trim();
    (control_type.eq_ignore_ascii_case(CONTROL_TYPE_WINDOW)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_LIST)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP))
        && node.name.trim() == UI_LABEL_GROUP_MEMBER_LIST
}

/// 判断完整 UI 快照树中是否呈现了非离屏的右侧 `"群成员列表"` 侧边栏
pub fn has_group_member_sidebar(root: &UiNodeSnapshot) -> bool {
    find_member_list_container(root).is_some()
}

/// 判断平铺节点快照切片中是否存在非离屏的右侧 `"群成员列表"` 侧边栏
pub fn has_group_member_sidebar_in_snapshots(snapshots: &[&UiNodeSnapshot]) -> bool {
    snapshots.iter().any(|snap| is_member_list_container(snap))
}

fn find_member_list_container(root: &UiNodeSnapshot) -> Option<&UiNodeSnapshot> {
    if is_member_list_container(root) {
        return Some(root);
    }
    for child in &root.children {
        if let Some(found) = find_member_list_container(child) {
            return Some(found);
        }
    }
    None
}

fn unwrap_member_rows_parent(container: &UiNodeSnapshot) -> Vec<&UiNodeSnapshot> {
    // 真实 QQNT 树结构为 `Window "群成员列表"` 下包裹一层视口 `Group`，再平铺各行成员 `Group` 节点
    let visible_children: Vec<&UiNodeSnapshot> = container
        .children
        .iter()
        .filter(|child| !child.is_offscreen && !profile::is_system_chrome(child))
        .collect();

    if visible_children.len() == 1
        && visible_children[0]
            .control_type
            .trim()
            .eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
        && visible_children[0].name.trim().is_empty()
        && !visible_children[0].children.is_empty()
    {
        return visible_children[0]
            .children
            .iter()
            .filter(|child| !child.is_offscreen && !profile::is_system_chrome(child))
            .collect();
    }

    visible_children
}

fn is_sidebar_noise_text(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty()
        || trimmed == UI_ROLE_OWNER_LABEL
        || trimmed == UI_ROLE_ADMIN_LABEL
        || trimmed == UI_NOISE_LEVEL_PREFIX
        || trimmed.contains("搜索群成员")
        || trimmed == SEARCH_KEYWORD
        || MEMBER_SIDEBAR_COUNT_REGEX.is_match(trimmed)
        || LEVEL_BADGE_REGEX.is_match(trimmed)
}

fn collect_row_texts<'a>(node: &'a UiNodeSnapshot, out: &mut Vec<&'a str>) {
    if node.is_offscreen || profile::is_system_chrome(node) {
        return;
    }
    let control_type = node.control_type.trim();
    if control_type.eq_ignore_ascii_case(CONTROL_TYPE_EDIT)
        || control_type.eq_ignore_ascii_case(CONTROL_TYPE_BUTTON)
    {
        return;
    }
    if control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT) {
        let text = node.name.trim();
        if !text.is_empty() {
            out.push(text);
        }
        return;
    }
    if node.children.is_empty() && !node.name.trim().is_empty() {
        out.push(node.name.trim());
    }
    for child in &node.children {
        collect_row_texts(child, out);
    }
}

/// 根据成员行或纵坐标分桶内的文本片段解析标准角色标识
fn resolve_member_role(texts: &[&str]) -> &'static str {
    if texts.contains(&UI_ROLE_OWNER_LABEL) {
        "owner"
    } else if texts.contains(&UI_ROLE_ADMIN_LABEL) {
        "admin"
    } else {
        "member"
    }
}

fn parse_member_row(row: &UiNodeSnapshot) -> Option<UpsertMemberInput> {
    let mut texts = Vec::new();
    collect_row_texts(row, &mut texts);
    if texts.is_empty() {
        return None;
    }

    let role = resolve_member_role(&texts);

    let nickname = texts
        .into_iter()
        .find(|text| !is_sidebar_noise_text(text))?
        .to_string();

    Some(UpsertMemberInput {
        nickname,
        real_user_id: None,
        card: String::new(),
        role: role.to_string(),
    })
}

/// 从 QQNT UI 快照树的右侧 `"群成员列表"` 容器提取可见成员及群主/管理员角色列表
pub fn extract_members_from_tree(root: &UiNodeSnapshot) -> Vec<UpsertMemberInput> {
    let Some(container) = find_member_list_container(root) else {
        return Vec::new();
    };
    let mut rows = unwrap_member_rows_parent(container);
    // QQNT 成员行在 UIA 树中可能并非按视觉纵坐标顺序列出，按矩形顶边排序保证顺序稳定
    rows.sort_by_key(|row| (row.rect[1], row.rect[0]));

    let mut members = Vec::new();
    let mut seen_nicknames: HashSet<String> = HashSet::new();

    for row in rows {
        let control_type = row.control_type.trim();
        let is_row = control_type.eq_ignore_ascii_case(CONTROL_TYPE_GROUP)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_LIST_ITEM)
            || control_type.eq_ignore_ascii_case(CONTROL_TYPE_TEXT);
        if !is_row {
            continue;
        }
        if let Some(member) = parse_member_row(row) {
            if seen_nicknames.insert(member.nickname.clone()) {
                members.push(member);
            }
        }
    }

    members
}

/// 从平铺采集快照切片中的 `"群成员列表"` 矩形区域兜底提取成员名称与角色信息
pub fn extract_members_from_snapshots(snapshots: &[&UiNodeSnapshot]) -> Vec<UpsertMemberInput> {
    let Some(container) = snapshots
        .iter()
        .copied()
        .filter(|snapshot| {
            is_member_list_container(snapshot) && profile::rect_is_valid(snapshot.rect)
        })
        .max_by_key(|snapshot| profile::rect_area(snapshot.rect))
    else {
        return Vec::new();
    };

    let mut texts: Vec<(i32, i32, &str)> = snapshots
        .iter()
        .copied()
        .filter(|snapshot| {
            !snapshot.is_offscreen
                && !profile::is_system_chrome(snapshot)
                && snapshot
                    .control_type
                    .trim()
                    .eq_ignore_ascii_case(CONTROL_TYPE_TEXT)
                && profile::rect_contains(container.rect, snapshot.rect)
                && !snapshot.name.trim().is_empty()
        })
        .map(|snapshot| (snapshot.rect[1], snapshot.rect[0], snapshot.name.trim()))
        .collect();
    texts.sort_by_key(|(top_y, left_x, _)| (*top_y, *left_x));

    // 将纵坐标相近（差值 <= 24px）的文本归并到同一成员行中
    let mut row_buckets: Vec<(i32, Vec<&str>)> = Vec::new();
    for (top_y, _, text) in texts {
        if let Some((bucket_y, items)) = row_buckets.last_mut() {
            if (top_y - *bucket_y).abs() <= 24 {
                items.push(text);
                continue;
            }
        }
        row_buckets.push((top_y, vec![text]));
    }

    let mut members = Vec::new();
    let mut seen_nicknames = HashSet::new();
    for (_, bucket_texts) in row_buckets {
        let role = resolve_member_role(&bucket_texts);
        let Some(nickname) = bucket_texts
            .into_iter()
            .find(|text| !is_sidebar_noise_text(text))
        else {
            continue;
        };
        if seen_nicknames.insert(nickname.to_string()) {
            members.push(UpsertMemberInput {
                nickname: nickname.to_string(),
                real_user_id: None,
                card: String::new(),
                role: role.to_string(),
            });
        }
    }

    members
}
