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
    Agent, AgentConfig, AgentEvent, AgentHooks, LlmConfig, Message, Result, ThinkingConfig, Tool,
    ToolDefinition, ToolRegistry,
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
    let thinking = match std::env::var("LLM_THINKING_BUDGET") {
        Ok(value) => {
            let budget_tokens = value.trim().parse::<u32>().map_err(|error| {
                kunpeng::llm::Error::InvalidConfig(format!(
                    "LLM_THINKING_BUDGET 不是合法整数: {error}"
                ))
            })?;
            Some(ThinkingConfig { budget_tokens })
        }
        Err(_) => None,
    };
    let agent_config = AgentConfig {
        thinking,
        server_tools: match std::env::var("LLM_SERVER_TOOLS") {
            Ok(value) if !value.trim().is_empty() => {
                serde_json::from_str::<Vec<serde_json::Value>>(value.trim()).map_err(|error| {
                    kunpeng::llm::Error::InvalidConfig(format!(
                        "LLM_SERVER_TOOLS 不是合法 JSON 数组: {error}"
                    ))
                })?
            }
            _ => Vec::new(),
        },
        ..AgentConfig::default()
    };
    let mut agent = Agent::with_history(model, tools, agent_config, vec![system.clone()]);

    // 一次 assistant 回复可能包含多个增量。打印完增量后，用最终 Assistant 事件
    // 补一个换行；如果关闭流式，则在这里打印完整内容。
    let saw_delta = Arc::new(AtomicBool::new(false));
    let saw_thinking = Arc::new(AtomicBool::new(false));
    let thinking_emitted = Arc::new(AtomicBool::new(false));
    let on_event = {
        let saw_delta = Arc::clone(&saw_delta);
        let saw_thinking = Arc::clone(&saw_thinking);
        let thinking_emitted = Arc::clone(&thinking_emitted);
        move |event: AgentEvent| match event {
            AgentEvent::AssistantDelta { text } => {
                // thinking 先于正文到达；第一段正文前结束 thinking 行。
                if saw_thinking.swap(false, Ordering::Relaxed) {
                    println!();
                }
                print!("{text}");
                let _ = io::stdout().flush();
                saw_delta.store(true, Ordering::Relaxed);
            }
            AgentEvent::ThinkingDelta { text } => {
                if !saw_thinking.swap(true, Ordering::Relaxed) {
                    print!("[thinking] ");
                }
                thinking_emitted.store(true, Ordering::Relaxed);
                print!("{text}");
                let _ = io::stdout().flush();
            }
            AgentEvent::Assistant {
                content,
                thinking,
                citations,
                server_tool_uses,
                ..
            } => {
                if saw_thinking.swap(false, Ordering::Relaxed) {
                    println!();
                } else if !thinking_emitted.swap(false, Ordering::Relaxed) {
                    if let Some(thinking) = thinking {
                        // 非流式（stream=false）时没有 ThinkingDelta，在这里补显示。
                        println!("[thinking] {thinking}");
                    }
                }
                thinking_emitted.store(false, Ordering::Relaxed);
                if saw_delta.swap(false, Ordering::Relaxed) {
                    println!();
                } else if let Some(content) = content {
                    println!("assistant: {content}");
                }
                for tool in &server_tool_uses {
                    println!("[server tool] {} {}", tool.name, tool.input);
                }
                if !citations.is_empty() {
                    println!("[citations] {}", citations.len());
                }
            }
            AgentEvent::ToolStarted { call } => {
                println!("tool started: {} {}", call.name, call.arguments);
            }
            AgentEvent::ToolFinished { execution } => {
                println!("tool finished: {} {:?}", execution.name, execution.outcome);
            }
            AgentEvent::Error { message } => {
                if saw_thinking.swap(false, Ordering::Relaxed)
                    || saw_delta.swap(false, Ordering::Relaxed)
                {
                    println!();
                }
                thinking_emitted.store(false, Ordering::Relaxed);
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
    println!("可选环境变量: LLM_THINKING_BUDGET, LLM_SERVER_TOOLS");
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
