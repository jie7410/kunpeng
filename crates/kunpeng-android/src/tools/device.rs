//! 设备工具：在线排查（仅在配置了真实 DeviceConnector 时注册）。
//!
//! `android_device_shell` 是高风险工具：宿主必须通过 `AgentHooks::with_approver`
//! 挂审批并白名单命令，否则不要把它暴露给模型。

use async_trait::async_trait;
use kunpeng_core::types::ToolDefinition;
use kunpeng_core::Result;
use std::sync::Arc;

use crate::capability::DeviceConnector;

/// `android_device_list`：列出当前连接的设备。
pub struct DeviceListTool {
    connector: Arc<dyn DeviceConnector>,
}

impl DeviceListTool {
    pub fn new(connector: Arc<dyn DeviceConnector>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for DeviceListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "android_device_list".into(),
            description: "列出当前连接的 Android 设备（serial、状态、型号、API level）。\
                          后续设备类工具用 serial 指定目标设备。"
                .into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    async fn call(&self, _arguments: serde_json::Value) -> Result<String> {
        let devices = self.connector.devices().await?;
        Ok(serde_json::json!({ "devices": devices }).to_string())
    }
}

/// `android_device_shell`：在设备上执行 shell 命令（需宿主审批）。
pub struct DeviceShellTool {
    connector: Arc<dyn DeviceConnector>,
}

impl DeviceShellTool {
    pub fn new(connector: Arc<dyn DeviceConnector>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl kunpeng_tools::Tool for DeviceShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "android_device_shell".into(),
            description: "在指定设备上执行 adb shell 命令并返回输出。优先使用只读诊断命令\
                          （dumpsys、pm list packages、cat /data/anr/ 等）；命令执行前需通过宿主审批。".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "serial": {"type": "string", "description": "目标设备 serial（来自 android_device_list）"},
                    "command": {"type": "string", "description": "shell 命令，避免交互式命令"}
                },
                "required": ["serial", "command"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let serial = super::required_str(&arguments, "android_device_shell", "serial")?;
        let command = super::required_str(&arguments, "android_device_shell", "command")?;
        let output = self.connector.shell(&serial, &command).await?;
        Ok(format!("# adb -s {serial} shell {command}\n{output}"))
    }
}
