//! Agent 组装：预设 profile + system prompt + 工具选择。
//!
//! profile 决定 system prompt 与校验（在线模式必须已配置设备连接器）；
//! 工具集由 [`crate::env::AndroidEnv`] 的能力决定。后续“修复建议 → 编译
//! 验证 → 复测”的迭代策略以 `AgentStrategy` 实现接入时，组装入口不变。

use kunpeng_agents::Agent;
use kunpeng_core::{ChatModel, Error, Message, Result};
use kunpeng_runtime::RunConfig;

use crate::env::AndroidEnv;

/// 诊断场景预设。区别主要体现在 system prompt 的作业流程上；工具集跟随
/// env 的能力配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AndroidProfile {
    /// 崩溃分诊：解析日志 → 提取堆栈 → 定位源码 → 输出根因分析。
    CrashTriage,
    /// 代码优化：面向指定代码区域的分析与改进建议（当前只读，建议以 diff 描述）。
    CodeOptimize,
    /// 在线排查：接入设备（adb）后的实时诊断；要求已配置 `DeviceConnector`。
    OnlineDiagnosis,
}

impl AndroidProfile {
    /// 生成 system prompt。拼入 env 的实际能力描述，让模型知道当前数据
    /// 来源与边界（离线/在线），而不是靠猜测。
    pub fn system_prompt(&self, env: &AndroidEnv) -> String {
        let sources = format!(
            "- 日志会话：log_sessions 查看已打开会话；log_open 可继续打开日志文件\n- 代码索引：{}",
            env.code().describe()
        );
        let device_section = if env.device_tools_enabled() {
            "- 设备连接：已配置（android_device_list 查看设备，android_device_shell 执行命令；\
               优先只读命令，命令需宿主审批）"
        } else {
            "- 设备连接：未配置（离线模式，没有设备类工具）"
        };

        match self {
            Self::CrashTriage => format!(
                "你是 Android 崩溃分诊助手。任务：从日志中识别崩溃，定位到源码，给出根因分析。\n\
                 \n\
                 数据来源：\n{sources}\n{device_section}\n\
                 \n\
                 作业流程：\n\
                 1. android_crashes 扫描会话拿崩溃/ANR 目录（signature、行号、次数）；\
                 陌生日志先 log_stats 看全貌，再决定检索方向。\n\
                 2. 从目录行号出发，log_grep 的 context 视图看崩溃前后上下文；\
                 需要更大范围用 log_read 按行号读原文。\n\
                 3. 优先检查应用帧（app_frames）：android_code_search 按类名定位文件，\
                 android_code_read 读取 file:line 附近上下文（建议前后各 20 行）。\n\
                 4. 结合 Caused by 链与源码给出根因假设。\n\
                 5. 补充证据：进程生死与查杀看 android_event_log_decode（name=am_）；\
                 疑似内存压力看 android_perf_signals 的 LMK/GC 信号。\n\
                 \n\
                 输出要求：\n\
                 - 结论先行：崩溃类别、根因假设、置信度（高/中/低）。\n\
                 - 每个结论必须给出证据：引用 path:line（代码）或 行号/signature/堆栈帧（日志）。\n\
                 - 无法定位时明确说“证据不足”，列出缺失的信息，不要编造行号。\n\
                 - 你当前是只读的：给出修复建议时描述修改点（path:line + 改法），不要声称已修改代码。"
            ),
            Self::CodeOptimize => format!(
                "你是 Android 代码优化助手。任务：面向给定代码区域做分析，输出可执行的优化建议。\n\
                 \n\
                 数据来源：\n{sources}\n{device_section}\n\
                 \n\
                 作业流程：\n\
                 1. 用 android_code_search / android_code_read 收集目标代码与调用点。\n\
                 2. 如有运行期日志：android_perf_signals 看启动耗时/掉帧/StrictMode/GC/LMK，\
                 android_crashes 看崩溃/ANR，log_template_cluster 看重复模式（高频重试往往是性能问题线索）。\n\
                 3. 有两份日志（如修复前后、正常机 vs 问题机）时，用 log_open 分别打开、\
                 log_sessions 改标签，再 log_diff 对比差异。\n\
                 4. 从正确性、性能（主线程/内存/电量）、可维护性三个维度评估。\n\
                 \n\
                 输出要求：\n\
                 - 每条建议包含：问题、证据（path:line / 行号）、改法（可用 diff 风格描述）、风险与权衡。\n\
                 - 区分“有日志证据”与“代码审阅推断”两类结论。\n\
                 - 只读模式：不要声称已修改代码。"
            ),
            Self::OnlineDiagnosis => format!(
                "你是 Android 在线排查助手。任务：在已连接的设备上采集现场信息并诊断问题。\n\
                 \n\
                 数据来源：\n{sources}\n{device_section}\n\
                 \n\
                 作业流程：\n\
                 1. android_device_list 确认目标设备。\n\
                 2. 需要系统状态时用 android_device_shell 执行只读命令（dumpsys、pm list、\
                 cat /data/anr/、logcat -d 导出等）；命令可能被宿主审批拒绝，被拒后换只读方案。\n\
                 3. 导出的日志用 log_open 建会话，android_crashes 拿崩溃目录，\
                 log_grep/log_read 检索定位；多份导出（main/events/kernel）用 log_cross_timeline 对齐。\n\
                 4. 启动慢/开机问题用 android_boot_timeline 看里程碑卡点、\
                 android_perf_signals 看 Displayed 耗时与掉帧。\n\
                 5. 定位到代码后用 android_code_search / android_code_read 交叉验证。\n\
                 \n\
                 输出要求：\n\
                 - 结论先行 + 证据（path:line / 行号 / signature / 命令输出摘录）。\n\
                 - 破坏性命令（pm clear、am force-stop 等）只在明确说明影响并必要时请求。\n\
                 - 只读模式：不要声称已修改设备状态或代码。"
            ),
        }
    }
}

/// 组装一个 Android 诊断 Agent。
///
/// 校验 profile 与能力配置的一致性（在线模式必须有设备连接器），
/// 生成工具注册表并写入 profile 的 system prompt。模型由宿主构建后传入，
/// 本 crate 不依赖 provider 层。
pub fn build_agent<M: ChatModel + 'static>(
    model: M,
    env: &AndroidEnv,
    profile: AndroidProfile,
    config: RunConfig,
) -> Result<Agent> {
    if profile == AndroidProfile::OnlineDiagnosis && !env.device_tools_enabled() {
        return Err(Error::InvalidConfig(
            "OnlineDiagnosis 需要设备连接器：先通过 AndroidEnvBuilder::with_device_connector 注入"
                .into(),
        ));
    }
    let tools = env.tool_registry()?;
    Ok(Agent::with_history(
        model,
        tools,
        config,
        vec![Message::system(profile.system_prompt(env))],
    ))
}
