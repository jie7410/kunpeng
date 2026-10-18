use async_trait::async_trait;
use kunpeng::llm::{
    Agent, AgentConfig, AgentEvent, AgentHooks, LlmConfig, Message, Result, Tool, ToolDefinition,
    ToolRegistry,
};

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "回显输入文本".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "要回显的文本" }
                },
                "required": ["text"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<String> {
        Ok(arguments
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = LlmConfig::from_env()?;
    let model = config.build()?;

    let mut tools = ToolRegistry::new();
    tools.register(EchoTool)?;

    let mut agent = Agent::with_history(
        model,
        tools,
        AgentConfig::default(),
        vec![Message::system("你是一个可以调用工具的助手。")],
    );

    let on_event = |event: AgentEvent| match event {
        AgentEvent::Assistant {
            content: Some(text),
            ..
        } => println!("assistant: {text}"),
        AgentEvent::ToolFinished { execution } => {
            println!("tool {}: {:?}", execution.name, execution.outcome);
        }
        _ => {}
    };
    let hooks = AgentHooks {
        on_event: Some(&on_event),
        approve: None,
    };

    let stop = agent.send(Message::user("请回显 hello"), &hooks).await?;
    println!("stop: {stop:?}");
    Ok(())
}
