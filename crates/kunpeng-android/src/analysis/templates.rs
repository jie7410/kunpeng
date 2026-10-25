//! 模板聚类（Drain-lite）：把 message 中的数字/十六进制 token 泛化成占位符，
//! 相同模板的行聚为一类，输出高频模板排行。
//!
//! 用途：海量日志先看模板分布再定位异常；`log_diff`（阶段二）也复用这里的
//! 模板作为对比签名。

use crate::types::LogLine;

/// 一类模板的统计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateStat {
    /// `TAG: masked message`；tag 缺失时只有 message。
    pub template: String,
    pub count: usize,
    pub first_line: u32,
    pub last_line: u32,
    /// 该模板的一条样例原文。
    pub sample: String,
}

/// 泛化单个 token：纯十进制数字 → `<N>`；十六进制（`0x...` 或 ≥6 位 hex 且含数字）
/// → `<H>`；其余原样。
fn mask_token(token: &str) -> String {
    if !token.is_empty() && token.chars().all(|c| c.is_ascii_digit()) {
        return "<N>".into();
    }
    let is_hex = |text: &str| !text.is_empty() && text.chars().all(|c| c.is_ascii_hexdigit());
    if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        if is_hex(hex) {
            return "<H>".into();
        }
    }
    if token.len() >= 6 && is_hex(token) && token.chars().any(|c| c.is_ascii_digit()) {
        return "<H>".into();
    }
    token.to_string()
}

/// 模板化一条 message：按非字母数字边界切 token，分隔符原样保留。
pub fn template_of(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut token = String::new();
    for character in message.chars() {
        if character.is_ascii_alphanumeric() {
            token.push(character);
        } else {
            if !token.is_empty() {
                output.push_str(&mask_token(&token));
                token.clear();
            }
            output.push(character);
        }
    }
    if !token.is_empty() {
        output.push_str(&mask_token(&token));
    }
    output
}

fn line_template(line: &LogLine) -> String {
    let message = line.message.as_deref().unwrap_or(line.raw.as_str());
    match &line.tag {
        Some(tag) => format!("{tag}: {}", template_of(message)),
        None => template_of(message),
    }
}

/// 聚类并按 count 降序返回前 `top` 个模板。
pub fn cluster(lines: &[LogLine], top: usize, min_count: usize) -> Vec<TemplateStat> {
    let mut stats: Vec<TemplateStat> = Vec::new();
    for line in lines {
        let template = line_template(line);
        let line_no = line.line_no.unwrap_or(0);
        match stats.iter_mut().find(|stat| stat.template == template) {
            Some(stat) => {
                stat.count += 1;
                stat.first_line = stat.first_line.min(line_no);
                stat.last_line = stat.last_line.max(line_no);
            }
            None => stats.push(TemplateStat {
                template,
                count: 1,
                first_line: line_no,
                last_line: line_no,
                sample: line.raw.clone(),
            }),
        }
    }
    stats.retain(|stat| stat.count >= min_count);
    stats.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.template.cmp(&b.template))
    });
    stats.truncate(top);
    stats
}
