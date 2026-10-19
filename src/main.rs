//! CLI 示例 / 组装层。
//!
//! 核心在 `kunpeng::llm` library 中；本文件只负责配置、示例工具和事件输出，
//! 将来 Tauri 入口可以完全替换本文件而复用同一套 `Agent` API。
//!
//! 这是一个交互式测试入口：
//! - 输入内容回车，发送给 Agent；
//! - 流式文本会直接打印；
//! - `/exit` 退出，`/clear` 清空历史，`/retry` 重试上一次失败的请求。

use async_trait::async_trait;
use kunpeng::llm::{
    Agent, AgentConfig, AgentEvent, AgentHooks, LlmConfig, Message, Result, Tool, ToolDefinition,
    ToolRegistry,
};
use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::io::AsyncBufReadExt;

const SYSTEM_PROMPT: &str = "你是一个可以调用工具的助手。回答前先判断是否需要工具。";

const HELP: &str = "\
命令：
  /help    显示帮助
  /clear   清空会话历史（保留 system prompt）
  /retry   重试当前历史（用于上一次请求失败）
  /exit    退出
  /quit    退出";

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

    let system = Message::system(SYSTEM_PROMPT);
    let mut agent = Agent::with_history(model, tools, AgentConfig::default(), vec![system.clone()]);

    // 一次 assistant 回复可能包含多个增量。打印完增量后，用最终 Assistant 事件
    // 补一个换行；如果关闭流式，则在这里打印完整内容。
    let saw_delta = Arc::new(AtomicBool::new(false));
    let on_event = {
        let saw_delta = Arc::clone(&saw_delta);
        move |event: AgentEvent| match event {
            AgentEvent::AssistantDelta { text } => {
                print!("{text}");
                let _ = io::stdout().flush();
                saw_delta.store(true, Ordering::Relaxed);
            }
            AgentEvent::Assistant {
                content: Some(content),
                ..
            } => {
                if saw_delta.swap(false, Ordering::Relaxed) {
                    println!();
                } else {
                    println!("assistant: {content}");
                }
            }
            AgentEvent::ToolStarted { call } => {
                println!("tool started: {} {}", call.name, call.arguments);
            }
            AgentEvent::ToolFinished { execution } => {
                println!("tool finished: {} {:?}", execution.name, execution.outcome);
            }
            AgentEvent::Error { message } => {
                if saw_delta.swap(false, Ordering::Relaxed) {
                    println!();
                }
                eprintln!("error: {message}");
            }
            // Finished 的 stop_reason 由 send/resume 的返回值统一打印。
            AgentEvent::Finished { .. } => {}
            _ => {}
        }
    };
    let hooks = AgentHooks {
        on_event: Some(&on_event),
        approve: None,
    };

    println!("Kunpeng interactive CLI");
    println!("{HELP}");
    println!();

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        print!("you> ");
        let _ = io::stdout().flush();

        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                eprintln!("读取输入失败: {error}");
                break;
            }
        };
        let input = line.trim();

        if input.is_empty() {
            continue;
        }

        match input {
            "/exit" | "/quit" | "exit" | "quit" => break,
            "/help" | "/?" => println!("{HELP}"),
            "/clear" => {
                agent.set_history(vec![system.clone()]);
                println!("history cleared");
            }
            "/retry" => match agent.resume(&hooks).await {
                Ok(stop_reason) => println!("[stop: {stop_reason:?}]"),
                Err(_) => println!("[request failed]"),
            },
            _ => match agent.send(Message::user(input), &hooks).await {
                Ok(stop_reason) => println!("[stop: {stop_reason:?}]"),
                Err(_) => println!("[request failed; history kept, use /retry to retry]"),
            },
        }
    }

    println!("bye");
    Ok(())
}
