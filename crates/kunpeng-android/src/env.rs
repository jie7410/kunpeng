//! 能力装配：`AndroidEnv` 持有一组能力实现，并生成工具注册表。
//!
//! 这是“离线 → 在线”升级的唯一改动点：换掉注入的 `LogSessionStore` /
//! `DeviceConnector`，工具与 Agent 不感知。

use std::path::Path;
use std::sync::Arc;

use kunpeng_core::{Error, Result};
use kunpeng_tools::ToolRegistry;

use crate::builtin::{
    AnrParser, FsCodeIndex, InMemorySessions, JavaCrashParser, StaticLogProvider,
    UnavailableDeviceConnector,
};
use crate::capability::{
    CodeIndex, CompositeLogParser, DeviceConnector, LogParser, LogProvider, LogSessionStore,
};
use crate::tools::cluster::TemplateClusterTool;
use crate::tools::code::{CodeReadTool, CodeSearchTool};
use crate::tools::crash::{CrashCatalogTool, LogParseTool};
use crate::tools::device::{DeviceListTool, DeviceShellTool};
use crate::tools::log::{LogGrepTool, LogOpenTool, LogReadTool, LogSessionsTool, LogStatsTool};

/// 一套已装配的诊断能力。
///
/// 构造后不可变（能力实现自身的内部可变性除外），可以被多个 Agent 共享；
/// 工具注册表每次调用 [`AndroidEnv::tool_registry`] 现生成，便于宿主追加
/// 自有工具。
pub struct AndroidEnv {
    /// 预留的实时日志源（adb 在线时经 `DeviceLogProvider` 接入）。
    /// v1 的检索/统计/聚类都作用在会话上，不走这个接口。
    logs: Arc<dyn LogProvider>,
    parser: Arc<CompositeLogParser>,
    code: Arc<dyn CodeIndex>,
    sessions: Arc<dyn LogSessionStore>,
    devices: Arc<dyn DeviceConnector>,
    /// 是否注册设备工具；只有配置了真实连接器才为 true。
    device_tools: bool,
}

impl AndroidEnv {
    pub fn logs(&self) -> &Arc<dyn LogProvider> {
        &self.logs
    }

    pub fn parser(&self) -> &Arc<CompositeLogParser> {
        &self.parser
    }

    pub fn code(&self) -> &Arc<dyn CodeIndex> {
        &self.code
    }

    pub fn sessions(&self) -> &Arc<dyn LogSessionStore> {
        &self.sessions
    }

    pub fn devices(&self) -> &Arc<dyn DeviceConnector> {
        &self.devices
    }

    pub fn device_tools_enabled(&self) -> bool {
        self.device_tools
    }

    /// 生成 Android 工具注册表。
    ///
    /// 日志家族（open/sessions/stats/grep/read/template_cluster）+ 崩溃工具 +
    /// 代码工具总是注册；设备 2 个工具仅在配置了真实连接器时注册，避免离线
    /// 模式下模型调用必然失败的工具。
    pub fn tool_registry(&self) -> Result<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register(LogOpenTool::new(self.sessions.clone()))?;
        registry.register(LogSessionsTool::new(self.sessions.clone()))?;
        registry.register(LogStatsTool::new(self.sessions.clone()))?;
        registry.register(LogGrepTool::new(self.sessions.clone()))?;
        registry.register(LogReadTool::new(self.sessions.clone()))?;
        registry.register(TemplateClusterTool::new(self.sessions.clone()))?;
        registry.register(LogParseTool::new(self.parser.clone()))?;
        registry.register(CrashCatalogTool::new(
            self.sessions.clone(),
            self.parser.clone(),
        ))?;
        registry.register(CodeSearchTool::new(self.code.clone()))?;
        registry.register(CodeReadTool::new(self.code.clone()))?;
        if self.device_tools {
            registry.register(DeviceListTool::new(self.devices.clone()))?;
            registry.register(DeviceShellTool::new(self.devices.clone()))?;
        }
        Ok(registry)
    }
}

/// [`AndroidEnv`] 的 builder。日志会话与代码索引必须显式提供（或用
/// [`AndroidEnvBuilder::offline`] 一次性装齐），缺项在 `build` 时报错。
pub struct AndroidEnvBuilder {
    logs: Option<Arc<dyn LogProvider>>,
    parsers: Vec<Box<dyn LogParser>>,
    code: Option<Arc<dyn CodeIndex>>,
    sessions: Option<Arc<dyn LogSessionStore>>,
    devices: Arc<dyn DeviceConnector>,
    device_tools: bool,
}

impl AndroidEnvBuilder {
    pub fn new() -> Self {
        Self {
            logs: None,
            parsers: vec![Box::new(JavaCrashParser), Box::new(AnrParser)],
            code: None,
            sessions: None,
            devices: Arc::new(UnavailableDeviceConnector),
            device_tools: false,
        }
    }

    /// 离线便捷入口：预载日志会话 + 文件系统代码索引 + 默认解析器。
    ///
    /// 日志同时进入会话（供 log_* 工具检索）和静态 provider（预留）。
    pub fn offline(repo_root: impl AsRef<Path>, logs: &str) -> Result<Self> {
        let sessions = Arc::new(InMemorySessions::new(Some(repo_root.as_ref()))?);
        sessions.preload("默认日志", logs)?;
        Ok(Self::new()
            .with_log_provider(Arc::new(StaticLogProvider::from_text("离线日志", logs)))
            .with_sessions(sessions)
            .with_code_index(Arc::new(FsCodeIndex::new(repo_root.as_ref())?)))
    }

    pub fn with_log_provider(mut self, provider: Arc<dyn LogProvider>) -> Self {
        self.logs = Some(provider);
        self
    }

    pub fn with_sessions(mut self, sessions: Arc<dyn LogSessionStore>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// 在默认解析器（Java/ANR）之后追加，例如后续的 tombstone 解析器。
    pub fn add_parser(mut self, parser: Box<dyn LogParser>) -> Self {
        self.parsers.push(parser);
        self
    }

    pub fn with_code_index(mut self, index: Arc<dyn CodeIndex>) -> Self {
        self.code = Some(index);
        self
    }

    /// 注入设备连接器，同时启用 `android_device_*` 工具的注册。
    ///
    /// 在线排查落地后，从设备导出的 logcat 经会话（log_open）进入同一套
    /// log_* 工具；实时 tail 是后续独立 trait。
    pub fn with_device_connector(mut self, connector: Arc<dyn DeviceConnector>) -> Self {
        self.devices = connector;
        self.device_tools = true;
        self
    }

    pub fn build(self) -> Result<AndroidEnv> {
        let logs = self.logs.ok_or_else(|| {
            Error::InvalidConfig(
                "缺少日志源：用 AndroidEnvBuilder::with_log_provider 或 offline 提供".into(),
            )
        })?;
        let code = self.code.ok_or_else(|| {
            Error::InvalidConfig(
                "缺少代码索引：用 AndroidEnvBuilder::with_code_index 或 offline 提供".into(),
            )
        })?;
        let sessions = self.sessions.ok_or_else(|| {
            Error::InvalidConfig(
                "缺少日志会话存储：用 AndroidEnvBuilder::with_sessions 或 offline 提供".into(),
            )
        })?;
        let parser = Arc::new(
            self.parsers
                .into_iter()
                .fold(CompositeLogParser::new(), |composite, parser| {
                    composite.with_parser(parser)
                }),
        );
        Ok(AndroidEnv {
            logs,
            parser,
            code,
            sessions,
            devices: self.devices,
            device_tools: self.device_tools,
        })
    }
}

impl Default for AndroidEnvBuilder {
    fn default() -> Self {
        Self::new()
    }
}
