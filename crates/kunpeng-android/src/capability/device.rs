//! 设备连接接缝：在线排查（adb）的预留入口。
//!
//! 当前离线模式使用 [`crate::builtin::UnavailableDeviceConnector`]（所有调用
//! 返回配置错误）。在线排查落地时的实现路径：
//!
//! 1. `AdbConnector` 实现 `DeviceConnector`：spawn `adb devices -l`、
//!    `adb -s <serial> shell <command>`、`adb -s <serial> logcat -d`，
//!    并负责进程生命周期与输出上限。
//! 2. 用 [`DeviceLogProvider`] 把 connector 的 logcat 能力适配成
//!    [`crate::capability::LogProvider`]，替换离线日志源——工具
//!    `android_log_fetch` 与模型看到的接口完全不变。
//! 3. 通过 [`crate::env::AndroidEnvBuilder::with_device_connector`] 注入，
//!    `android_device_list` / `android_device_shell` 工具随之启用。
//!
//! `shell` 是高风险操作：宿主应通过 `AgentHooks::with_approver` 挂审批，
//! 白名单化允许的命令（dumpsys、pm list 等只读命令优先）。

use async_trait::async_trait;
use kunpeng_core::Result;
use std::sync::Arc;

use super::log::LogProvider;
use crate::types::{DeviceInfo, LogQuery, LogSlice};

/// 与设备集合交互的能力，语义对齐 adb 子命令。
#[async_trait]
pub trait DeviceConnector: Send + Sync {
    /// 列出当前可见设备，语义对齐 `adb devices -l`。
    async fn devices(&self) -> Result<Vec<DeviceInfo>>;

    /// 在指定设备上执行 shell 命令并返回 stdout。
    ///
    /// 实现应内置输出上限（超限截断并标注），并拒绝交互式命令；
    /// 是否放行由宿主的审批层决定，本 trait 不做策略判断。
    async fn shell(&self, serial: &str, command: &str) -> Result<String>;

    /// 拉取指定设备的 logcat（语义对齐 `logcat -d`，一次性导出）。
    async fn logcat(&self, serial: &str, query: &LogQuery) -> Result<LogSlice>;
}

/// 把 [`DeviceConnector::logcat`] 适配成 [`LogProvider`]。
///
/// 这是离线→在线切换的关键粘合：换掉这个 provider，`android_log_fetch`
/// 工具与提示词都不用改。
pub struct DeviceLogProvider {
    connector: Arc<dyn DeviceConnector>,
    serial: String,
}

impl DeviceLogProvider {
    pub fn new(connector: Arc<dyn DeviceConnector>, serial: impl Into<String>) -> Self {
        Self {
            connector,
            serial: serial.into(),
        }
    }
}

#[async_trait]
impl LogProvider for DeviceLogProvider {
    async fn fetch(&self, query: &LogQuery) -> Result<LogSlice> {
        self.connector.logcat(&self.serial, query).await
    }

    fn describe(&self) -> String {
        format!("设备 {} 的 logcat", self.serial)
    }
}
