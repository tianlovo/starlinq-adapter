//! 质量检验分级与 12 大测试类别结果模型定义

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 质量检验执行等级枚举（从 L0 快速冒烟至 L4 行业标准统计基准）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QaTier {
    /// L0 快速冒烟级别
    L0,
    /// L1 打桩与边界门禁级别
    L1,
    /// L2 并发与全链路集成级别
    L2,
    /// L3 压力/暴力/性能/内存/CPU 极限负载级别
    L3,
    /// L4 行业标准统计基准级别
    L4,
    /// 执行全量 L0 至 L4 检验
    All,
}

impl QaTier {
    /// 解析命令行传入的等级字符串（支持大小写无关的 `L0`-`L4` 与 `All`）
    pub fn parse_flag(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "L0" => Ok(Self::L0),
            "L1" => Ok(Self::L1),
            "L2" => Ok(Self::L2),
            "L3" => Ok(Self::L3),
            "L4" => Ok(Self::L4),
            "ALL" => Ok(Self::All),
            other => Err(format!(
                "不支持的 QA 等级参数 '{other}'，可选 L0|L1|L2|L3|L4|All"
            )),
        }
    }

    /// 返回规范化等级文本标签
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::L0 => "L0",
            Self::L1 => "L1",
            Self::L2 => "L2",
            Self::L3 => "L3",
            Self::L4 => "L4",
            Self::All => "All",
        }
    }

    /// 判断某测试分类等级在当前选定运行等级下是否应当执行
    pub fn includes(&self, target: Self) -> bool {
        *self == Self::All || *self == target
    }
}

/// 单条 QA 测试用例的执行状态、耗时与关键指标摘要
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseOutcome {
    pub name: String,
    pub passed: bool,
    pub skipped: bool,
    pub duration_ms: f64,
    pub detail: String,
}

/// 12 大 QA 测试类别之一的汇总报告（含 SLA 目标值、实测值及旗下全部用例详情）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryReport {
    pub id: String,
    pub category_zh: String,
    pub category_en: String,
    pub tier: String,
    pub passed: bool,
    pub total_cases: usize,
    pub passed_cases: usize,
    pub skipped_cases: usize,
    pub failed_cases: usize,
    pub duration_ms: f64,
    pub sla_target: String,
    pub sla_actual: String,
    pub key_metrics: BTreeMap<String, f64>,
    pub cases: Vec<CaseOutcome>,
}
