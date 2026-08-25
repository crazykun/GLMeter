use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 智谱开放平台 / Z.ai 的 API Key（id.secret 格式）
    pub api_key: String,
    /// 国内: https://open.bigmodel.cn  国际: https://api.z.ai
    pub base_url: String,
    /// 激活额度时使用的模型
    pub model: String,
    /// 激活请求的 max_tokens
    pub max_tokens: u32,
    /// 自动刷新间隔（秒）
    pub interval_secs: u64,
    /// 定时刷新的起始对齐时间点（"HH:MM"，本地时区）。
    /// 设置后刷新时刻对齐为「每天 HH:MM 起、每 interval_secs 一跳」的网格，
    /// 例如 00:00 + 300s → 00:00/00:05/00:10…；留空则从启动时刻滚动计时。
    #[serde(default)]
    pub refresh_align: Option<String>,
    /// 托盘显示文字/悬停提示模板，支持变量:
    /// {level} {5h_used} {5h_left} {5h_reset} {5h_countdown}
    /// {weekly_used} {weekly_left} {mcp_used} {mcp_total} {mcp_left}
    #[serde(default = "default_tray_title")]
    pub tray_title: String,
    /// 定时激活：额度窗口未激活（或重置后）时，自动发送最小请求
    /// 以触发 5h 窗口统计并获取 nextResetTime
    #[serde(default = "default_auto_activate")]
    pub auto_activate: bool,
}

fn default_tray_title() -> String {
    "GLM {5h_left}%".into()
}

fn default_auto_activate() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://open.bigmodel.cn".into(),
            model: "glm-5.2".into(),
            max_tokens: 8,
            interval_secs: 300,
            refresh_align: None,
            tray_title: default_tray_title(),
            auto_activate: default_auto_activate(),
        }
    }
}

impl Config {
    pub fn monitor_url(&self) -> String {
        format!(
            "{}/api/monitor/usage/quota/limit",
            self.base_url.trim_end_matches('/')
        )
    }

    pub fn chat_url(&self) -> String {
        format!(
            "{}/api/coding/paas/v4/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }

    pub fn configured(&self) -> bool {
        !self.api_key.trim().is_empty()
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("glmeter")
        .join("config.toml")
}

/// 读取配置；若文件不存在则写入默认模板，方便用户直接编辑。
/// 环境变量 GLM_API_KEY / GLM_BASE_URL 可覆盖文件内容。
pub fn load() -> (Config, PathBuf) {
    let path = config_path();
    let mut cfg: Config = fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();

    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let template = toml::to_string_pretty(&cfg).unwrap_or_default();
        let _ = fs::write(&path, template);
    }

    if let Ok(k) = std::env::var("GLM_API_KEY") {
        if !k.trim().is_empty() {
            cfg.api_key = k.trim().to_string();
        }
    }
    if let Ok(u) = std::env::var("GLM_BASE_URL") {
        if !u.trim().is_empty() {
            cfg.base_url = u.trim().to_string();
        }
    }

    (cfg, path)
}
