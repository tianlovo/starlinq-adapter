//! 拟人化发送节拍控制、发送冷却限流与结构化交互验证报告落盘工具

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;
use common_types::constants::RUNTIME_LATEST_REPORT_FILE;
use common_types::{
    AdapterError, AdapterStatus, InputMode, InteractionConfig, Result, SendOutcome,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use uia_core::TypingDelayRange;

const PRE_TYPING_MIN_MS: u64 = 180;
const PRE_TYPING_MAX_MS: u64 = 450;

/// 结构化交互验证报告默认落盘相对路径
pub const DEFAULT_REPORT_REL_PATH: &str = RUNTIME_LATEST_REPORT_FILE;

/// 持久化至运行目录的结构化验证与发送交互报告对象
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionReport {
    pub generated_at: String,
    pub action: String,
    pub group: Option<String>,
    pub detected_member_count: Option<u32>,
    pub used_mode: InputMode,
    pub foreground_touched: bool,
    pub mouse_touched: bool,
    pub verified_in_chat: bool,
    pub security_breaker_tripped: bool,
    pub status: Option<AdapterStatus>,
    pub send_outcome: Option<SendOutcome>,
    pub detail: String,
}

/// 出站发送冷却计时器与拟人化随机打字延时控制器
#[derive(Debug)]
pub struct Humanizer {
    min_send_interval: Duration,
    char_delay_min_ms: u64,
    char_delay_max_ms: u64,
    last_send_at: Option<Instant>,
}

impl Humanizer {
    /// 根据交互配置构造 `Humanizer` 控制器实例
    pub fn from_config(interaction_config: &InteractionConfig) -> Self {
        let min_char = interaction_config.char_delay_min_ms;
        let max_char = interaction_config.char_delay_max_ms.max(min_char);
        Self {
            min_send_interval: Duration::from_millis(interaction_config.min_send_interval_ms),
            char_delay_min_ms: min_char,
            char_delay_max_ms: max_char,
            last_send_at: None,
        }
    }

    /// 返回归一化后的单字符输入延时边界 `(min_ms, max_ms)`
    pub fn char_delay_bounds(&self) -> (u64, u64) {
        (self.char_delay_min_ms, self.char_delay_max_ms)
    }

    /// 构造供 `uia_core` 调用的单字符延时参数对象
    pub fn typing_delay_range(&self) -> TypingDelayRange {
        TypingDelayRange::new(self.char_delay_min_ms, self.char_delay_max_ms)
    }

    /// 计算距离下一次允许发送还需等待的冷却毫秒数
    pub fn remaining_cooldown_ms(&self) -> u64 {
        let Some(last) = self.last_send_at else {
            return 0;
        };
        let elapsed = last.elapsed();
        if elapsed >= self.min_send_interval {
            0
        } else {
            let remaining = self.min_send_interval.saturating_sub(elapsed);
            (remaining.as_millis() as u64).max(1)
        }
    }

    /// 校验最小发送间隔，若仍处于冷却期则返回限流错误 `AdapterError::RateLimited`
    pub fn check_cooldown(&self) -> Result<()> {
        let retry_after_ms = self.remaining_cooldown_ms();
        if retry_after_ms > 0 {
            return Err(AdapterError::RateLimited { retry_after_ms });
        }
        Ok(())
    }

    /// 记录一次成功或已受理的出站发送时间戳
    pub fn mark_sent(&mut self) {
        self.last_send_at = Some(Instant::now());
    }

    /// 采样 `180..=450ms` 区间的打字前拟人停顿时间
    pub fn sample_pre_typing_pause(&self) -> Duration {
        let millis = rand::thread_rng().gen_range(PRE_TYPING_MIN_MS..=PRE_TYPING_MAX_MS);
        Duration::from_millis(millis)
    }
}

/// 将结构化交互报告写入默认 `.runtime/latest-report.json` 路径
pub fn write_latest_report(report: &InteractionReport) -> Result<PathBuf> {
    let path = PathBuf::from(DEFAULT_REPORT_REL_PATH);
    write_report_to(&path, report)?;
    Ok(path)
}

/// 将结构化交互报告写入指定文件路径（自动创建缺失父目录）
pub fn write_report_to(path: &Path, report: &InteractionReport) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(report).map_err(|error| {
        AdapterError::UiaFailure(format!("failed to serialize report: {error}"))
    })?;
    std::fs::write(path, json)?;
    Ok(())
}

/// 根据发送结果与适配器状态构建 `send_group_message` 交互报告
pub fn build_send_report(outcome: &SendOutcome, status: &AdapterStatus) -> InteractionReport {
    InteractionReport {
        generated_at: Utc::now().to_rfc3339(),
        action: "send_group_message".to_string(),
        group: Some(outcome.group.clone()),
        detected_member_count: status.detected_member_count,
        used_mode: outcome.used_mode,
        foreground_touched: outcome.foreground_touched,
        mouse_touched: outcome.mouse_touched,
        verified_in_chat: outcome.verified_in_chat,
        security_breaker_tripped: status.security_breaker_tripped,
        status: Some(status.clone()),
        send_outcome: Some(outcome.clone()),
        detail: outcome.detail.clone(),
    }
}

/// 根据适配器当前状态与细节说明构建 `open_group` 交互报告
pub fn build_open_report(status: &AdapterStatus, detail: impl Into<String>) -> InteractionReport {
    InteractionReport {
        generated_at: Utc::now().to_rfc3339(),
        action: "open_group".to_string(),
        group: status.active_group.clone(),
        detected_member_count: status.detected_member_count,
        used_mode: status.input_mode,
        foreground_touched: false,
        mouse_touched: false,
        verified_in_chat: false,
        security_breaker_tripped: status.security_breaker_tripped,
        status: Some(status.clone()),
        send_outcome: None,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_and_pre_typing_bounds_work() {
        let interaction_config = InteractionConfig {
            min_send_interval_ms: 250,
            char_delay_min_ms: 40,
            char_delay_max_ms: 20, // 故意倒置上下限以验证归一化保底逻辑
            ..Default::default()
        };
        let mut humanizer = Humanizer::from_config(&interaction_config);
        assert_eq!(humanizer.char_delay_bounds(), (40, 40));
        assert!(humanizer.check_cooldown().is_ok());

        for _ in 0..10 {
            let pause = humanizer.sample_pre_typing_pause().as_millis() as u64;
            assert!((PRE_TYPING_MIN_MS..=PRE_TYPING_MAX_MS).contains(&pause));
        }

        humanizer.mark_sent();
        assert!(matches!(
            humanizer.check_cooldown(),
            Err(AdapterError::RateLimited { retry_after_ms }) if retry_after_ms > 0
        ));
    }

    #[test]
    fn writes_structured_report_to_explicit_path() {
        let temp_dir =
            std::env::temp_dir().join(format!("qqnt_driver_test_{}", std::process::id()));
        let report_path = temp_dir.join("latest-report.json");
        let outcome = SendOutcome {
            ok: true,
            group: "Demo测试群 (5)".to_string(),
            used_mode: InputMode::BackgroundUia,
            foreground_touched: false,
            mouse_touched: false,
            verified_in_chat: true,
            elapsed_ms: 120,
            detail: "ok".to_string(),
            ..Default::default()
        };
        let status = AdapterStatus {
            active_group: Some("Demo测试群 (5)".to_string()),
            detected_member_count: Some(5),
            ..Default::default()
        };
        let report = build_send_report(&outcome, &status);
        write_report_to(&report_path, &report).expect("write report");
        let raw = std::fs::read_to_string(&report_path).expect("read report");
        let parsed: InteractionReport = serde_json::from_str(&raw).expect("parse report");
        assert_eq!(parsed.detected_member_count, Some(5));
        assert!(!parsed.mouse_touched);
        assert!(!parsed.foreground_touched);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
