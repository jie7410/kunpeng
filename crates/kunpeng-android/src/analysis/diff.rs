//! 双会话差异对比：以模板为签名，比较两份日志的模板集合与频次。
//!
//! 典型用法：问题机 vs 正常机、修复前 vs 修复后。new/missing 定位
//! “只在一侧出现的模式”，changed 定位“两侧都有但频次剧变”的模式。

use super::templates::{self, TemplateStat};

/// 模板频次变化（两侧都出现）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateDelta {
    pub template: String,
    pub baseline: usize,
    pub target: usize,
    pub delta: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReport {
    /// 只在 baseline 出现（模板, 次数）。
    pub missing_in_target: Vec<(String, usize)>,
    /// 只在 target 出现。
    pub new_in_target: Vec<(String, usize)>,
    /// 两侧都有且次数不同，按 |delta| 降序。
    pub changed: Vec<TemplateDelta>,
}

const MAX_PER_SECTION: usize = 50;

/// 对比两份日志。排序规则与 [`super::templates::cluster`] 一致（count 降序、
/// 模板升序），保证结果可复现。
pub fn diff_sessions(baseline: &[TemplateStat], target: &[TemplateStat]) -> DiffReport {
    let mut missing_in_target = Vec::new();
    let mut new_in_target = Vec::new();
    let mut changed = Vec::new();

    for base in baseline {
        match target.iter().find(|stat| stat.template == base.template) {
            None => missing_in_target.push((base.template.clone(), base.count)),
            Some(found) => {
                if found.count != base.count {
                    changed.push(TemplateDelta {
                        template: base.template.clone(),
                        baseline: base.count,
                        target: found.count,
                        delta: found.count as i64 - base.count as i64,
                    });
                }
            }
        }
    }
    for stat in target {
        if !baseline.iter().any(|base| base.template == stat.template) {
            new_in_target.push((stat.template.clone(), stat.count));
        }
    }

    missing_in_target.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    new_in_target.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    changed.sort_by(|a, b| {
        b.delta
            .abs()
            .cmp(&a.delta.abs())
            .then_with(|| a.template.cmp(&b.template))
    });
    missing_in_target.truncate(MAX_PER_SECTION);
    new_in_target.truncate(MAX_PER_SECTION);
    changed.truncate(MAX_PER_SECTION);

    DiffReport {
        missing_in_target,
        new_in_target,
        changed,
    }
}

/// 便捷入口：直接从行数据聚成模板再对比。
pub fn diff_lines(
    baseline: &[crate::types::LogLine],
    target: &[crate::types::LogLine],
) -> DiffReport {
    // usize::MAX 让 cluster 不截断；min_count=1 保留全部模板。
    diff_sessions(
        &templates::cluster(baseline, usize::MAX, 1),
        &templates::cluster(target, usize::MAX, 1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LogLine;

    fn parse_all(text: &str) -> Vec<LogLine> {
        text.lines()
            .enumerate()
            .map(|(i, raw)| LogLine::parse_numbered(raw, i as u32 + 1))
            .collect()
    }

    #[test]
    fn reports_new_missing_and_changed() {
        let baseline = parse_all(
            "10-25 08:00:00.001  1  1 D App: retry 1 failed\n\
             10-25 08:00:00.002  1  1 D App: retry 2 failed\n\
             10-25 08:00:00.003  1  1 D App: cache hit rate 0.9",
        );
        let target = parse_all(
            "10-25 08:10:00.001  1  1 D App: retry 1 failed\n\
             10-25 08:10:00.002  1  1 D App: retry 2 failed\n\
             10-25 08:10:00.003  1  1 D App: retry 3 failed\n\
             10-25 08:10:00.004  1  1 D App: retry 4 failed\n\
             10-25 08:10:00.005  1  1 D App: retry 5 failed",
        );

        let report = diff_lines(&baseline, &target);
        // cache 行只在 baseline。
        assert_eq!(report.missing_in_target.len(), 1);
        assert!(report.missing_in_target[0].0.contains("cache"));
        // retry 频次 2 -> 5。
        assert_eq!(report.changed.len(), 1);
        assert_eq!(report.changed[0].baseline, 2);
        assert_eq!(report.changed[0].target, 5);
        assert_eq!(report.changed[0].delta, 3);
        // 没有只出现在 target 的新模板（retry 已存在）。
        assert!(report.new_in_target.is_empty());
    }
}
