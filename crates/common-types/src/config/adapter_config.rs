//! 顶层适配器总配置定义

use serde::{Deserialize, Serialize};

use super::{
    BotConfig, InteractionConfig, OneBotConfig, QqClientConfig, SchedulerConfig, ServerConfig,
    StorageConfig,
};
use crate::messaging::GroupTargetSpec;

/// 从 TOML 文件加载并可被命令行参数覆盖的适配器顶层配置
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AdapterConfig {
    pub server: ServerConfig,
    pub target: GroupTargetSpec,
    pub interaction: InteractionConfig,
    pub bot: BotConfig,
    pub qq_client: QqClientConfig,
    pub storage: StorageConfig,
    pub onebot: OneBotConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
}
