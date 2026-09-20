// doctor.rs — 能力诊断：说明 pi 版缓存优化在 phi 上的可落地情况。
//
// pi 版 pi-cache-optimizer 的核心优化全部挂在宿主提供的钩子上，而 phi 的
// Rust SDK 目前不暴露这些能力。本模块把「哪些能做、哪些不能做、为什么」
// 结构化输出，避免用户以为扩展静默失效。

use crate::config::CacheOptimizerConfig;

/// 单项能力的可落地性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applicability {
    /// 已生效。
    Active,
    /// phi 缺少对应钩子，无法实现。
    Unsupported,
}

/// 一条能力诊断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    /// pi 版的能力名。
    pub name: &'static str,
    /// 可落地性。
    pub applicability: Applicability,
    /// phi 侧的原因或替代做法。
    pub detail: &'static str,
}

/// 全部能力诊断项。
pub fn capabilities() -> Vec<Capability> {
    vec![
        Capability {
            name: "系统提示词前缀稳定化（stable candidates 前置）",
            applicability: Applicability::Unsupported,
            detail: "phi 的 before_agent_start 只提供用户提示词，不暴露系统提示词与其组成项，\
                     无法重排前缀。",
        },
        Capability {
            name: "skills 列表压缩（<available_skills> 一索引化）",
            applicability: Applicability::Unsupported,
            detail: "依赖系统提示词内容与 skills 清单，phi 均未暴露。",
        },
        Capability {
            name: "session-overview 去抖（提交/工作区/行数抖动字段）",
            applicability: Applicability::Unsupported,
            detail: "该块位于系统提示词内，phi 未暴露系统提示词。",
        },
        Capability {
            name: "prompt_cache_key / cache retention 注入",
            applicability: Applicability::Unsupported,
            detail: "phi 没有 before_provider_request / before_provider_headers 钩子，\
                     扩展无法改写请求体或请求头。",
        },
        Capability {
            name: "缓存命中率 / token 速率统计",
            applicability: Applicability::Active,
            detail: "读取宿主持久化的会话 JSONL（<phi_home>/session/<cwd>/<id>.jsonl）\
                     里的 usage，用 phi-ext-common::stats 计算命中率与 token 速率，\
                     由 /cache-optimizer stats 展示，并通过 ctx.set_status 同步到\
                     宿主底部状态行（token 状态栏旁的扩展状态区）。",
        },
        Capability {
            name: "配置持久化与能力诊断（/cache-optimizer）",
            applicability: Applicability::Active,
            detail: "由本扩展自行实现：配置写在 ~/.phi/extensions/phi-cache-optimizer/config.json。",
        },
    ]
}

/// 渲染诊断报告。
pub fn report(config: &CacheOptimizerConfig) -> String {
    let mut lines = vec![
        "\u{1F50D} phi-cache-optimizer 能力诊断".to_string(),
        String::new(),
        format!(
            "启用：{} · footer 口径：{:?} · prompt_cache_key：{:?}",
            if config.enabled { "是" } else { "否" },
            config.footer_mode,
            config.prompt_cache_key
        ),
        String::new(),
    ];

    let capabilities = capabilities();
    let unsupported = capabilities
        .iter()
        .filter(|capability| capability.applicability == Applicability::Unsupported)
        .count();

    for capability in &capabilities {
        let mark = match capability.applicability {
            Applicability::Active => "✅",
            Applicability::Unsupported => "⛔",
        };
        lines.push(format!("{mark} {}", capability.name));
        lines.push(format!("    {}", capability.detail));
    }

    lines.push(String::new());
    if unsupported == 0 {
        lines.push("全部能力均已生效。".to_string());
    } else {
        lines.push(format!(
            "共 {unsupported} 项能力因 phi 未暴露对应钩子而无法生效；\
             扩展不会静默失败，此报告即为现状说明。"
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_should_cover_both_states() {
        let capabilities = capabilities();
        assert!(capabilities
            .iter()
            .any(|capability| capability.applicability == Applicability::Active));
        assert!(capabilities
            .iter()
            .any(|capability| capability.applicability == Applicability::Unsupported));
    }

    #[test]
    fn report_should_list_every_capability() {
        let report = report(&CacheOptimizerConfig::default());
        for capability in capabilities() {
            assert!(report.contains(capability.name), "报告缺少 {}", capability.name);
            assert!(report.contains(capability.detail), "报告缺少说明 {}", capability.name);
        }
    }

    #[test]
    fn report_should_summarise_unsupported_count() {
        let report = report(&CacheOptimizerConfig::default());
        let unsupported = capabilities()
            .iter()
            .filter(|capability| capability.applicability == Applicability::Unsupported)
            .count();
        assert!(report.contains(&format!("共 {unsupported} 项能力")));
    }

    #[test]
    fn report_should_show_config_summary() {
        let config = CacheOptimizerConfig {
            enabled: false,
            ..CacheOptimizerConfig::default()
        };
        let report = report(&config);
        assert!(report.contains("启用：否"), "got {report}");
    }
}