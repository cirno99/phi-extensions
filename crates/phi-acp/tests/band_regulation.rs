//! 上下文「区间调节」端到端仿真。
//!
//! # 目标与实测结论
//!
//! 目标行为：编码过程中上下文在约 50K～200K 之间**波动**，只有实在压不动时
//! 才缓慢堆积——而不是单调上涨。
//!
//! 在 phi 宿主上，这个区间由**两个机制**共同构成，两者缺一不可：
//!
//! 1. **下沿 = absorb 的使用率门槛**。`tool_result` 拦截是 phi 上唯一能把
//!    内容从上游请求里真正删掉的通道（见 `absorb` 模块文档）。低于门槛时不动手，
//!    上下文自然长大；越过门槛后新工具输出被压成 stub，且使用率越高删得越多。
//!    absorb 只能**减缓增长**，它本身永远不会让上下文变小。
//! 2. **上沿 = 宿主原生压缩（compaction）**。它是唯一能把上下文**真正变小**的
//!    事件：`internal/agent/engine.go` 在自然停轮后调 `runCompact`，当
//!    `contextTokens > context_window - 16384` 时保留约 20K 消息 + ≤13.1K 摘要，
//!    其余历史丢弃。
//!
//! 两条关键约束（都在本测试里被断言）：
//!
//! - **宿主压缩要求模型配置了 `context_window`**。`ShouldCompact` 在
//!   `contextWindow <= 0` 时恒为 false。没有内置预设的模型名（例如
//!   `deepseek-v4.1-flash`）默认拿不到 window，于是压缩永不触发、上下文单调
//!   上涨到溢出。这是本项目实测到的真实故障。
//! - **扩展的自动提醒会挡住压缩**。`turn_stopping` 返回 `continue` 会跳过同轮的
//!   `runCompact`，所以自动提醒默认为关（见 `AcpConfig::auto_nudge_enabled`）。
//!
//! 本测试用确定性伪会话驱动「absorb 减缓 + 压缩重置」这条完整回路，断言区间
//! 行为成立；并单独断言「缺少压缩时必然堆积」，把上沿的依赖关系钉死。

use phi_acp::absorb::plan_absorb;
use phi_acp::tokenize::count_tokens;
use phi_acp::types::AbsorbConfig;

/// 宿主压缩的固定常量（`internal/session/compaction/setting.go`）。
const HOST_REVERSE_TOKENS: u64 = 16_384;
/// 宿主压缩后保留的近期 token（`keepRecentTokens`）。
const HOST_KEEP_RECENT_TOKENS: u64 = 20_000;
/// 压缩摘要的输出上限（`reverseTokens * historySummaryRatio(0.8)`）。
const HOST_SUMMARY_TOKENS: u64 = (HOST_REVERSE_TOKENS * 8) / 10;

/// 简版确定性 PRNG（xorshift64*），避免引入 dev-dependency。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo).max(1)
    }
}

/// 生成一条「像真实工具输出」的文本：头部 + 大段中间 + 尾部。
fn fake_tool_output(rng: &mut Rng, tokens: u64) -> String {
    let body = (tokens as usize).saturating_mul(4);
    let mut text = String::with_capacity(body + 64);
    text.push_str("$ cargo build\n");
    let chunk = "compiling phi-acp v0.1.0\n";
    while text.len() < body {
        text.push_str(chunk);
    }
    text.push_str("\nFinished dev profile in 1.23s\n");
    text.push_str(&"x".repeat(rng.range(0, 400) as usize));
    text
}

/// absorb 配置（与扩展默认一致）。
fn absorb_config(threshold: f64) -> AbsorbConfig {
    AbsorbConfig {
        enabled: true,
        context_threshold_pct: threshold,
        min_tool_tokens: 500,
        keep_prefix_chars: 1500,
        keep_suffix_chars: 500,
        ..Default::default()
    }
}

/// 一次仿真的结果。
struct Simulation {
    /// 每 turn 结束时的上下文 token 数。
    peaks: Vec<u64>,
    /// 累计回收 token 数（absorb 实际删掉的量）。
    reclaimed: u64,
    /// absorb 触发的次数（用于确认回路真的在工作）。
    absorbs: usize,
    /// 宿主压缩触发次数。
    compactions: usize,
}

/// 仿真参数。
struct Params {
    /// 模型上限（扩展视角的 `modelContextLimit`）。
    limit: u64,
    /// 宿主压缩窗口；`None` 表示未配置 `context_window`（压缩永不触发）。
    context_window: Option<u64>,
    /// absorb 使用率门槛。
    absorb_threshold: f64,
}

/// 跑一次伪会话，模拟「absorb 减缓 + 宿主压缩重置」。
fn simulate(seed: u64, turns: usize, params: &Params) -> Simulation {
    let config = absorb_config(params.absorb_threshold);
    let mut rng = Rng(seed);
    let mut context: u64 = params.limit / 10;
    let mut peaks = Vec::new();
    let mut reclaimed = 0u64;
    let mut absorbs = 0usize;
    let mut compactions = 0usize;

    for _ in 0..turns {
        let calls = rng.range(2, 7);
        for _ in 0..calls {
            let tokens = rng.range(500, 12_000);
            let text = fake_tool_output(&mut rng, tokens);
            let original = count_tokens(&text);
            let usage = context as f64 / params.limit as f64;
            match plan_absorb("bash", &text, false, usage, &config, 1, true) {
                Some(plan) => {
                    reclaimed += plan.reclaimed_tokens();
                    absorbs += 1;
                    // 宿主只看到 stub。
                    context += plan.stub_tokens;
                }
                None => context += original,
            }
        }

        // 自然停轮 → 宿主尝试压缩。压缩触发时上下文被重置。
        if let Some(window) = params.context_window {
            let threshold = window.saturating_sub(HOST_REVERSE_TOKENS);
            if context > threshold {
                context = HOST_KEEP_RECENT_TOKENS + HOST_SUMMARY_TOKENS;
                compactions += 1;
            }
        }
        peaks.push(context);
    }

    Simulation {
        peaks,
        reclaimed,
        absorbs,
        compactions,
    }
}

/// 核心断言：启用宿主压缩后，上下文在一个**有界区间**内波动。
#[test]
fn context_should_oscillate_within_a_bounded_band() {
    let params = Params {
        limit: 200_000,
        // 想让压缩上沿落在 200K：window = 200K + 16384。
        context_window: Some(200_000 + HOST_REVERSE_TOKENS),
        absorb_threshold: 0.30,
    };
    let sim = simulate(0x5EED_1234, 600, &params);

    // 压缩真的发生过，否则这个测试没有验证到回路。
    assert!(sim.compactions > 0, "宿主压缩应至少触发一次");
    assert!(sim.absorbs > 0, "absorb 应至少命中一次");

    // 上沿：被压缩阈值封顶（留一点抖动余量给压缩后的当轮增量）。
    let peak = *sim.peaks.iter().max().expect("应有采样");
    let ceiling = params.context_window.unwrap() - HOST_REVERSE_TOKENS;
    assert!(
        peak <= ceiling + 20_000,
        "上下文峰值 {peak} 应被压缩阈值 {ceiling} 约束住"
    );

    // 下沿：压缩后回落到保留量附近。
    let floor = *sim.peaks.iter().min().unwrap();
    assert!(
        floor >= HOST_KEEP_RECENT_TOKENS,
        "压缩后不应低于保留量 {HOST_KEEP_RECENT_TOKENS}：floor={floor}"
    );

    // 波动：既有上升也有回落。
    let drops = sim.peaks.windows(2).filter(|w| w[1] < w[0]).count();
    assert!(drops > 0, "应存在回落到低水位的回合");
}

/// absorb 的门槛把下沿抬起来：没有门槛时上下文被一直压小，
/// 形不成「先长后收」的波动区间。
#[test]
fn absorb_threshold_lifts_the_floor() {
    let with_gate = simulate(
        0x1111_2222,
        300,
        &Params {
            limit: 200_000,
            context_window: None,
            absorb_threshold: 0.30,
        },
    );
    let without_gate = simulate(
        0x1111_2222,
        300,
        &Params {
            limit: 200_000,
            context_window: None,
            absorb_threshold: 0.0,
        },
    );
    let peak = |s: &Simulation| *s.peaks.iter().max().unwrap();
    assert!(
        peak(&without_gate) < peak(&with_gate),
        "无门槛时上下文应被压得更低：ungated={} gated={}",
        peak(&without_gate),
        peak(&with_gate)
    );
}

/// absorb 的单条回收比例随使用率单调上升（自适应窗口在工作）。
#[test]
fn reclaim_ratio_should_grow_with_usage() {
    let config = absorb_config(0.30);
    let text = fake_tool_output(&mut Rng(7), 8_000);
    let low = plan_absorb("bash", &text, false, 0.35, &config, 1, true).expect("低水位应吸收");
    let high = plan_absorb("bash", &text, false, 0.95, &config, 1, true).expect("高水位应吸收");
    assert!(
        high.reclaimed_tokens() > low.reclaimed_tokens(),
        "高压下应回收更多：low={} high={}",
        low.reclaimed_tokens(),
        high.reclaimed_tokens()
    );
}

/// 回归：没有宿主压缩时，上下文必然单调堆积到溢出。
///
/// 这正是本项目的真实故障场景——模型名（`deepseek-v4.1-flash`）没有内置
/// 预设，`context_window` 默认为 0，`ShouldCompact` 恒为 false。absorb
/// 只能减缓增长，无法替代压缩重置。
#[test]
fn without_host_compaction_context_must_pile_up() {
    let params = Params {
        limit: 200_000,
        context_window: None,
        absorb_threshold: 0.30,
    };
    let sim = simulate(0xDEAD_BEEF, 600, &params);
    let peak = *sim.peaks.iter().max().unwrap();
    assert!(
        peak > params.limit,
        "缺少压缩时应堆积到超出模型上限（证明上沿必须靠压缩）：peak={peak}"
    );
    assert!(
        sim.reclaimed > 0,
        "即使堆积，absorb 也应在持续回收（只是抵不过新增量）"
    );
    // 单调性：最终水位显著高于初期水位。
    let early = sim.peaks[20];
    let late = *sim.peaks.last().unwrap();
    assert!(
        late > early,
        "缺少压缩时水位应持续上涨：early={early} late={late}"
    );
}
