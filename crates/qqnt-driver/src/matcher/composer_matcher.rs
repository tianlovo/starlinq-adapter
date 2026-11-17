//! 右下角聊天消息输入编辑框与“发送”按钮控件定位器（剔除侧栏微型伪目标）

use common_types::{AdapterError, Result, UiNodeSnapshot};
use uia_core::CollectedNode;

use crate::profile;

/// 从快照引用切片中按启发性评分定位右下角输入框与发送按钮索引
pub fn locate_composer_and_send_indices(
    snapshots: &[&UiNodeSnapshot],
    win_rect: [i32; 4],
    is_standalone: bool,
) -> Result<(usize, Option<usize>)> {
    let eff_rect = profile::effective_window_rect_from_snapshots(win_rect, snapshots);
    let excluded_rects: Vec<[i32; 4]> = snapshots
        .iter()
        .copied()
        .filter(|snapshot| {
            profile::is_excluded_sidebar_container(snapshot)
                && profile::rect_is_valid(snapshot.rect)
        })
        .map(|snapshot| snapshot.rect)
        .collect();
    let in_excluded = |rect: [i32; 4]| {
        excluded_rects
            .iter()
            .any(|container| profile::rect_contains(*container, rect))
    };

    let editor_idx = snapshots
        .iter()
        .enumerate()
        .filter(|(_, snapshot)| !in_excluded(snapshot.rect))
        .filter_map(|(index, snapshot)| {
            profile::composer_score(snapshot, eff_rect, is_standalone).map(|score| (index, score))
        })
        .max_by_key(|(_, score)| *score)
        .map(|(index, _)| index)
        .ok_or_else(|| {
            AdapterError::UiaFailure(
                "could not locate bottom-right composer editor in attached window".to_string(),
            )
        })?;

    let send_idx = snapshots
        .iter()
        .enumerate()
        .filter(|(_, snapshot)| !in_excluded(snapshot.rect))
        .filter_map(|(index, snapshot)| {
            profile::send_button_score(snapshot, eff_rect, is_standalone)
                .map(|score| (index, score))
        })
        .max_by_key(|(_, score)| *score)
        .map(|(index, _)| index);

    Ok((editor_idx, send_idx))
}

/// 从平铺采集节点集合中定位右下角输入框与可选发送按钮索引
pub fn locate_composer_and_send_button(
    nodes: &[CollectedNode],
    win_rect: [i32; 4],
    is_standalone: bool,
) -> Result<(usize, Option<usize>)> {
    let snapshots: Vec<&UiNodeSnapshot> = nodes.iter().map(|node| &node.snapshot).collect();
    locate_composer_and_send_indices(&snapshots, win_rect, is_standalone)
}
