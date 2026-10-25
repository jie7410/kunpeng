//! Android 工具集：把能力接缝桥接成模型可调用的 `Tool`。
//!
//! 约定：
//! - 工具名：日志分析家族用 `log_*`，Android 语义工具用 `android_*`；
//! - 结构化结果用 JSON 输出（模型引用行号、路径更可靠）；
//! - 参数字段错误返回 `Error::InvalidTool`，由 Agent 转成 `is_error` 的
//!   tool_result 回传，模型可据此修正参数；
//! - 带 `session` 参数的工具缺省作用于活动会话。

pub mod cluster;
pub mod code;
pub mod crash;
pub mod cross;
pub mod device;
pub mod events;
pub mod log;
pub mod perf;

use kunpeng_core::{Error, Result};
use serde_json::Value;
use std::sync::Arc;

use crate::capability::LogSessionStore;
use crate::types::{LogLine, LogSessionMeta};

pub(crate) fn required_str(arguments: &Value, tool: &str, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::InvalidTool {
            name: tool.into(),
            reason: format!("缺少必填参数 {key}（字符串）"),
        })
}

pub(crate) fn optional_str(arguments: &Value, tool: &str, key: &str) -> Result<Option<String>> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid_tool(tool, &format!("{key} 必须是字符串"))),
    }
}

pub(crate) fn optional_u32(arguments: &Value, tool: &str, key: &str) -> Result<Option<u32>> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            let value = number
                .as_u64()
                .filter(|value| *value <= u32::MAX as u64)
                .ok_or_else(|| invalid_tool(tool, &format!("{key} 必须是 0..=u32::MAX 的整数")))?;
            Ok(Some(value as u32))
        }
        Some(_) => Err(invalid_tool(tool, &format!("{key} 必须是非负整数"))),
    }
}

pub(crate) fn invalid_tool(name: &str, reason: &str) -> Error {
    Error::InvalidTool {
        name: name.into(),
        reason: reason.into(),
    }
}

/// 解析 `session` 参数（缺省活动会话）并取行快照。
pub(crate) async fn resolve_session(
    store: &Arc<dyn LogSessionStore>,
    arguments: &Value,
    tool: &str,
) -> Result<(LogSessionMeta, Arc<Vec<LogLine>>)> {
    let requested = optional_str(arguments, tool, "session")?;
    let id = match requested {
        Some(id) => id,
        None => {
            store
                .active()
                .await
                .ok_or_else(|| invalid_tool(tool, "没有活动日志会话，先用 log_open 打开日志"))?
                .id
        }
    };
    let lines = store.snapshot(&id).await?;
    let meta = store
        .list()
        .await
        .into_iter()
        .find(|meta| meta.id == id)
        .ok_or_else(|| invalid_tool(tool, &format!("会话不存在：{id}")))?;
    Ok((meta, lines))
}
