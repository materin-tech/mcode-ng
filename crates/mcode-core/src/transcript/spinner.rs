//! Spinner：CC 的 `✻ Verb… (esc to interrupt · ↓ N tokens · 32s)` 状态行。
//!
//! - 帧（darwin 语义）：`· ✢ ✳ ✶ ✻ ✽` + 逆序回放，50ms/帧
//! - 动词每回合随机采样一次（CC ~186 个动词的等价子集）
//! - 状态 parts 顺序：esc 提示 → tokens（30s 后显示）→ 计时；宽度不够从尾部丢弃

use super::style::{compact_tokens, display_width, format_duration, Color, Span, StyledLine};

/// 帧 + 逆序回放（darwin CC 行为）。
pub const FRAMES: [&str; 10] = ["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];

/// CC 动词表的等价子集（拼写逐条取自 CC verbs 列表）。
pub const VERBS: [&str; 89] = [
    "Accomplishing",
    "Actioning",
    "Actualizing",
    "Baking",
    "Brewing",
    "Calculating",
    "Cancelling",
    "Channelling",
    "Churning",
    "Clauding",
    "Coalescing",
    "Cogitating",
    "Combobulating",
    "Computing",
    "Concocting",
    "Conjuring",
    "Considering",
    "Cooking",
    "Crafting",
    "Creating",
    "Crunching",
    "Crystallizing",
    "Deciphering",
    "Deliberating",
    "Determining",
    "Digesting",
    "Discombobulating",
    "Distilling",
    "Divining",
    "Doing",
    "Elucidating",
    "Enchanting",
    "Envisioning",
    "Fermenting",
    "Finagling",
    "Forging",
    "Forming",
    "Frolicking",
    "Generating",
    "Germinating",
    "Hatching",
    "Herding",
    "Honking",
    "Hustling",
    "Ideating",
    "Imagining",
    "Incubating",
    "Inferring",
    "Jiving",
    "Manifesting",
    "Marinating",
    "Meandering",
    "Moseying",
    "Mulling",
    "Mustering",
    "Musing",
    "Noodling",
    "Percolating",
    "Perusing",
    "Philosophising",
    "Pondering",
    "Pontificating",
    "Processing",
    "Puttering",
    "Puzzling",
    "Reticulating",
    "Ruminating",
    "Scheming",
    "Schlepping",
    "Shimmying",
    "Shucking",
    "Simmering",
    "Smooshing",
    "Spelunking",
    "Spinning",
    "Stewing",
    "Sussing",
    "Synthesizing",
    "Thinking",
    "Transmuting",
    "Unfurling",
    "Unravelling",
    "Vibing",
    "Wandering",
    "Whirring",
    "Wibbling",
    "Wizarding",
    "Working",
    "Wrangling",
];

/// 基于时钟的伪随机取样（选词场景足够，不引入 rand 依赖）。
pub fn time_nonce() -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.subsec_nanos() as usize) ^ ((d.as_secs() as usize) << 13)
}

/// 随机采样一个回合动词（每回合一次）。
pub fn sample_verb() -> &'static str {
    VERBS[time_nonce() % VERBS.len()]
}

/// 帧序号 → 帧字符。
pub fn frame_at(tick: u64) -> &'static str {
    FRAMES[(tick % FRAMES.len() as u64) as usize]
}

/// 组装 spinner 行；`width` 不够时按 parts 从尾部丢弃。
///
/// parts 语义对齐 CC：`(esc to interrupt` 永在；`↓ N tokens` 需 elapsed ≥ 30s；
/// 末尾 `Ns)` 计时最后被丢。`extra` 是附加片段（如 `thinking`）。
pub fn status_line(
    verb: &str,
    frame: &str,
    _tick: u64,
    elapsed_secs: u64,
    turn_tokens: u64,
    width: usize,
    extra: Option<&str>,
) -> StyledLine {
    let mut line = StyledLine::default();
    line.push(Span::colored(Color::Claude, frame));
    line.push(Span::new("  "));
    line.push(Span::new(format!("{verb}…")));

    // parts：从高优先到低；渲染时逐个尝试加入，宽度不足丢弃尾部
    #[derive(PartialEq)]
    enum Part {
        Interrupt,
        Tokens(u64),
        Extra(String),
        Time(u64),
    }
    let mut parts: Vec<Part> = vec![Part::Interrupt];
    if elapsed_secs >= 30 && turn_tokens > 0 {
        parts.push(Part::Tokens(turn_tokens));
    }
    if let Some(e) = extra {
        parts.push(Part::Extra(e.to_string()));
    }
    parts.push(Part::Time(elapsed_secs));

    for keep in (1..=parts.len()).rev() {
        let candidate = &parts[..keep];
        let mut text = String::from(" (");
        for (i, p) in candidate.iter().enumerate() {
            if i > 0 {
                text.push_str(" · ");
            }
            match p {
                Part::Interrupt => text.push_str("esc to interrupt"),
                Part::Tokens(n) => text.push_str(&format!("↓ {} tokens", compact_tokens(*n))),
                Part::Extra(e) => text.push_str(e),
                Part::Time(s) => text.push_str(&format_duration(*s)),
            }
        }
        text.push(')');
        if width == 0 || display_width(&line.text()) + display_width(&text) <= width {
            line.push(Span::dim_text(text));
            break;
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_cycle_with_replay() {
        assert_eq!(frame_at(0), "·");
        assert_eq!(frame_at(5), "✽");
        assert_eq!(frame_at(10), "·", "逆序回放后循环");
    }

    #[test]
    fn status_contains_verb_interrupt_and_time() {
        let line = status_line("Clauding", "✻", 0, 8, 0, 200, None);
        let t = line.text();
        assert!(t.starts_with("✻  Clauding… (esc to interrupt · 8s)"), "{t}");
    }

    #[test]
    fn tokens_appear_only_after_30s() {
        let early = status_line("Thinking", "✻", 0, 29, 15300, 200, None).text();
        assert!(!early.contains("tokens"), "{early}");
        let late = status_line("Thinking", "✻", 0, 31, 15300, 200, None).text();
        assert!(late.contains("↓ 15.3k tokens"), "{late}");
    }

    #[test]
    fn narrow_width_drops_tail_parts() {
        // 宽 40：只装得下 esc 提示，tokens 和时间被丢
        let line = status_line("Reticulating", "✻", 0, 100, 900_000, 40, None);
        let t = line.text();
        assert!(t.contains("esc to interrupt"), "{t}");
        assert!(!t.contains("tokens"), "{t}");
        assert!(!t.contains("1m 40s"), "{t}");
    }

    #[test]
    fn extra_part_inserted_before_time() {
        let line = status_line("Vibing", "✻", 0, 40, 5000, 200, Some("thinking"));
        let t = line.text();
        assert!(
            t.contains("↓ 5.0k tokens · thinking · 40s)"),
            "顺序：tokens → extra → 时间，{t}"
        );
    }

    #[test]
    fn verbs_are_sampled_from_table() {
        let v = sample_verb();
        assert!(VERBS.contains(&v), "{v} 必须来自动词表");
    }
}
