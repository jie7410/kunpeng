//! CLI 示例 / 组装层。
//!
//! 核心在 `kunpeng::llm` library 中；本文件只负责配置、示例工具和事件输出，
//! 将来 Tauri 入口可以完全替换本文件而复用同一套 `Agent` API。

use async_trait::async_trait;
use kunpeng::llm::{
    Agent, AgentConfig, AgentEvent, AgentHooks, LlmConfig, Message, Result, Tool, ToolDefinition,
    ToolRegistry,
};

/// 示例工具：查询天气。
struct GetWeatherTool;

#[async_trait]
impl Tool for GetWeatherTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "get_weather".into(),
            description: "查询某个城市的天气".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": {
                        "type": "string",
                        "description": "城市名，例如 北京"
                    }
                },
                "required": ["city"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        let city = arguments
            .get("city")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        Ok(format!("{city} 今天晴，气温 26°C，西北风 2 级"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = LlmConfig::from_env()?;
    let model = config.build()?;

    let mut tools = ToolRegistry::new();
    tools.register(GetWeatherTool)?;

    let mut agent = Agent::with_history(
        model,
        tools,
        AgentConfig::default(),
        vec![Message::system(
            "你是一个可以调用工具的助手。回答前先判断是否需要工具。",
        )],
    );

    let on_event = |event: AgentEvent| match event {
        AgentEvent::Assistant {
            content: Some(content),
            ..
        } => println!("assistant: {content}"),
        AgentEvent::ToolStarted { call } => {
            println!("tool started: {} {}", call.name, call.arguments);
        }
        AgentEvent::ToolFinished { execution } => {
            println!("tool finished: {} {:?}", execution.name, execution.outcome);
        }
        AgentEvent::Finished { stop_reason } => {
            println!("finished: {stop_reason:?}");
        }
        AgentEvent::Error { message } => {
            eprintln!("error: {message}");
        }
        _ => {}
    };
    let hooks = AgentHooks {
        on_event: Some(&on_event),
        approve: None,
    };

    let stop_reason = agent
        .send(Message::user("北京今天天气如何？"), &hooks)
        .await?;

    println!("stop reason: {stop_reason:?}");
    Ok(())
}
