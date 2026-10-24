//! Token 估算：CJK 感知 + usage 真值滚动校准。
//!
//! 旧版 `chars/4` 对中文低估约 2 倍，导致 80% 压缩阈值迟迟不触发、直接撞
//! context 上限。新方案分两层：
//! 1. **raw 估算**：CJK 字符按 ~1 token/字、其余按 4 字符/token 分桶计算，
//!    天然对中英混排给出保守（偏高）估计——宁可早压缩，不可晚触发；
//! 2. **校准层**：每轮 API 返回的 `usage.input_tokens` 是真值锚点，用
//!    `actual / raw` 的 EMA 修正系数滚动校准，越用越准。

use crate::message::{ContentBlock, Message};

/// CJK（含中文、日文假名、韩文谚文、全角标点）每字符 token 数。
/// 实测主流 tokenizer 在 0.7~1.4 之间，取 1.0 保守居中。
pub const CJK_TOKENS_PER_CHAR: f64 = 1.0;

/// 拉丁/代码类每 token 字符数（经典 chars/4 口径）。
pub const ASCII_CHARS_PER_TOKEN: f64 = 4.0;

/// 每条消息的固定结构开销（role、分隔符、tool_call 包装等）。
pub const PER_MESSAGE_OVERHEAD_TOKENS: f64 = 8.0;

/// 图片的平坦视觉预算（视觉 token 粗估，与分辨率无关的保守值）。
pub const IMAGE_TOKEN_BUDGET: f64 = 1500.0;

pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2EFF   // CJK 部首补充
        | 0x3000..=0x303F // CJK 标点
        | 0x3040..=0x309F // 平假名
        | 0x30A0..=0x30FF // 片假名
        | 0x3400..=0x4DBF // CJK 扩展 A
        | 0x4E00..=0x9FFF // CJK 统一表意
        | 0xAC00..=0xD7AF // 谚文音节
        | 0xF900..=0xFAFF // CJK 兼容表意
        | 0xFF00..=0xFFEF // 全角形式
        | 0x20000..=0x2A6DF // CJK 扩展 B
    )
}

/// 对一段文本做 raw 估算（未校准）。
pub fn raw_estimate_text(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    (cjk as f64 * CJK_TOKENS_PER_CHAR).ceil() as usize
        + (other as f64 / ASCII_CHARS_PER_TOKEN).ceil() as usize
}

/// 单条消息 raw 估算。
pub fn raw_estimate_message(msg: &Message) -> usize {
    let mut tokens = PER_MESSAGE_OVERHEAD_TOKENS;
    for block in &msg.content {
        match block {
            ContentBlock::Image { .. } => tokens += IMAGE_TOKEN_BUDGET,
            other => {
                if let Some(text) = other.as_text() {
                    tokens += raw_estimate_text(text) as f64;
                } else if let ContentBlock::ToolUse { name, input, .. } = other {
                    tokens += raw_estimate_text(name) as f64;
                    tokens += raw_estimate_text(&input.to_string()) as f64;
                }
            }
        }
    }
    tokens.ceil() as usize
}

/// 消息序列 raw 估算（未校准）。
pub fn raw_estimate_messages(messages: &[Message]) -> usize {
    messages.iter().map(raw_estimate_message).sum()
}

/// 带校准的估算器。
///
/// `correction` 是 `真实 input_tokens / raw 估算` 的指数滑动平均，
/// 初始 1.0，随真值样本收敛。线程边界：由 agent 循环独占持有。
#[derive(Clone, Debug)]
pub struct TokenEstimator {
    correction: f64,
    samples: u32,
}

impl Default for TokenEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenEstimator {
    pub fn new() -> Self {
        Self {
            correction: 1.0,
            samples: 0,
        }
    }

    pub fn correction(&self) -> f64 {
        self.correction
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// 校准后的估算值。
    pub fn estimate_messages(&self, messages: &[Message]) -> usize {
        let raw = raw_estimate_messages(messages) as f64;
        (raw * self.correction).ceil() as usize
    }

    pub fn estimate_text(&self, text: &str) -> usize {
        (raw_estimate_text(text) as f64 * self.correction).ceil() as usize
    }

    /// 喂入一轮真值：发送前对同一消息序列的 raw 估算 + API 返回的
    /// input_tokens。异常样本（比值出界）直接丢弃，防脏数据带偏系数。
    pub fn observe(&mut self, raw_estimate: usize, actual_input_tokens: u64) {
        if raw_estimate == 0 || actual_input_tokens == 0 {
            return;
        }
        let factor = actual_input_tokens as f64 / raw_estimate as f64;
        if !(0.2..=6.0).contains(&factor) {
            return; // 出界样本多为异常（重试错位/非文本注入），不采纳
        }
        let alpha = if self.samples < 3 { 0.5 } else { 0.2 };
        self.correction = self.correction * (1.0 - alpha) + factor * alpha;
        self.samples = self.samples.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Conversation;

    #[test]
    fn ascii_uses_chars_per_four() {
        assert_eq!(raw_estimate_text("abcdefgh"), 2);
        assert_eq!(raw_estimate_text(""), 0);
    }

    #[test]
    fn cjk_counts_per_char() {
        // 4 个汉字 ≈ 4 token（而非 chars/4 = 1）
        assert_eq!(raw_estimate_text("你好世界"), 4);
        assert_eq!(raw_estimate_text("你好世界world"), 4 + 2); // 4 CJK + 5 latin/4
    }

    #[test]
    fn fullwidth_punctuation_is_cjk() {
        assert_eq!(raw_estimate_text("你好，世界！"), 6);
    }

    #[test]
    fn message_estimate_includes_overhead() {
        let msg = Message::user_text("abcd"); // 1 raw
        assert_eq!(raw_estimate_message(&msg), 8 + 1);
    }

    #[test]
    fn image_has_flat_budget() {
        let msg = Message::new(
            crate::message::Role::User,
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
        );
        assert_eq!(raw_estimate_message(&msg), 8 + 1500);
    }

    #[test]
    fn calibration_converges_toward_truth() {
        let mut est = TokenEstimator::new();
        // 中文场景：raw 长期低估一半（actual ≈ 2x raw）
        for _ in 0..10 {
            let raw = raw_estimate_text("你好世界测试文本若干");
            est.observe(raw, (raw * 2) as u64);
        }
        assert!((est.correction() - 2.0).abs() < 0.1);
        let text = "一些中文内容";
        let calibrated = est.estimate_text(text);
        assert_eq!(
            calibrated,
            (raw_estimate_text(text) as f64 * 2.0).ceil() as usize
        );
    }

    #[test]
    fn out_of_bounds_samples_are_rejected() {
        let mut est = TokenEstimator::new();
        est.observe(100, 100_000); // factor 1000 → 拒绝
        est.observe(100, 1); // factor 0.01 → 拒绝
        assert_eq!(est.correction(), 1.0);
        assert_eq!(est.samples(), 0);
    }

    #[test]
    fn conversation_watermark_reads_last_usage() {
        use crate::message::{Role, Usage};
        let mut conv = Conversation::new();
        conv.push(Message::user_text("x"));
        let mut a = Message::new(Role::Assistant, vec![]);
        a.usage = Some(Usage {
            input_tokens: 1234,
            output_tokens: 10,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        });
        conv.push(a);
        assert_eq!(conv.last_context_usage().unwrap().input_tokens, 1234);
    }
}
