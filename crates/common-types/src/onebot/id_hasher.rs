//! 基于 FNV-1a 的确定性 OneBot 标识映射、群容量档位推断与规范群名比对工具函数

use std::hash::Hasher;

use fnv::FnvHasher;

use crate::constants::{
    MIN_ONEBOT_ID_BOUND, QQ_GROUP_TIER_1000, QQ_GROUP_TIER_200, QQ_GROUP_TIER_2000,
    QQ_GROUP_TIER_3000, QQ_GROUP_TIER_500,
};

/// 将字符串种子确定性映射为区间 [1, max_bound) 内的正整数 OneBot 标识
pub fn stable_onebot_id(seed: &str, max_bound: i64) -> i64 {
    // 直接向 FnvHasher 写入原始字节流以获得稳定的 64 位 FNV-1a 散列值
    let mut hasher = FnvHasher::default();
    hasher.write(seed.as_bytes());
    let hash_value = hasher.finish();

    let safe_bound = max_bound.max(MIN_ONEBOT_ID_BOUND) as u64;
    ((hash_value % (safe_bound - 1)) + 1) as i64
}

/// 根据已知群成员人数或显式上限推断 QQ 标准群容量上限（未获取任何人数时返回 0）
pub fn infer_qq_max_member_count(member_count: Option<i32>, max_member_count: Option<i32>) -> i32 {
    if let Some(explicit) = max_member_count.filter(|&val| val > 0) {
        return explicit;
    }
    let Some(current) = member_count.filter(|&val| val > 0) else {
        return 0;
    };
    if current <= QQ_GROUP_TIER_200 {
        QQ_GROUP_TIER_200
    } else if current <= QQ_GROUP_TIER_500 {
        QQ_GROUP_TIER_500
    } else if current <= QQ_GROUP_TIER_1000 {
        QQ_GROUP_TIER_1000
    } else if current <= QQ_GROUP_TIER_2000 {
        QQ_GROUP_TIER_2000
    } else {
        QQ_GROUP_TIER_3000
    }
}

/// 剥离群显示名末尾的成员人数后缀（如 ` (9)` 或 `（26人）`），保留规范群名主体
pub fn strip_group_count_suffix(raw: &str) -> String {
    let trimmed = raw.trim();
    let (without_close, open_delim) = if let Some(inner) = trimmed.strip_suffix(')') {
        (inner, '(')
    } else if let Some(inner) = trimmed.strip_suffix('）') {
        (inner, '（')
    } else {
        return trimmed.to_string();
    };

    if let Some((prefix, suffix_body)) = without_close.rsplit_once(open_delim) {
        let count_digits = suffix_body.trim().trim_end_matches('人').trim();
        let prefix_trimmed = prefix.trim();
        if !prefix_trimmed.is_empty()
            && !count_digits.is_empty()
            && count_digits.len() <= 5
            && count_digits.chars().all(|ch| ch.is_ascii_digit())
        {
            return prefix_trimmed.to_string();
        }
    }

    trimmed.to_string()
}

/// 精确比对两个群显示名称是否在剥离末尾成员数后缀后指向同一群聊
pub fn canonical_group_names_match(left: &str, right: &str) -> bool {
    let left_canon = strip_group_count_suffix(left);
    let right_canon = strip_group_count_suffix(right);
    !left_canon.is_empty() && left_canon == right_canon
}
