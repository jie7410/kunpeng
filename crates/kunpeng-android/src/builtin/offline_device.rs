//! 未配置设备连接器：离线模式的占位实现。

use async_trait::async_trait;
use kunpeng_core::{Error, Result};

use crate::capability::DeviceConnector;
use crate::types::{DeviceInfo, LogQuery, LogSlice};

/// 所有调用都返回配置错误，错误信息指向启用在线排查的路径。
///
/// 存在的意义：让 `AndroidEnv` 的设备字段永远有值，工具注册逻辑只需判断
/// “是否配置了真实连接器”，不用到处处理 `Option<Arc<dyn DeviceConnector>>`。
pub struct UnavailableDeviceConnector;

fn unavailable() -> Error {
    Error::InvalidConfig(
        "未配置设备连接：当前为离线模式。实现 DeviceConnector（例如 AdbConnector）并通过 AndroidEnvBuilder::with_device_connector 启用在线排查".into(),
    )
}

#[async_trait]
impl DeviceConnector for UnavailableDeviceConnector {
    async fn devices(&self) -> Result<Vec<DeviceInfo>> {
        Err(unavailable())
    }

    async fn shell(&self, _serial: &str, _command: &str) -> Result<String> {
        Err(unavailable())
    }

    async fn logcat(&self, _serial: &str, _query: &LogQuery) -> Result<LogSlice> {
        Err(unavailable())
    }
}
