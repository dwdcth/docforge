//! 语言检测处理能力（feature = "lang-detect"）。
//!
//! 设计吸收 document-parser 的限流采样思路（前 N 字符检测，见
//! THIRD_PARTY_NOTICES.md）：全量文本喂 whatlang 既慢也无必要。检测结果写
//! `RawDoc.meta["language"]`（ISO 639-3 码，如 "cmn" 中文普通话 / "eng"），
//! engine 通用搬运至 `DocMeta.extra["language"]`；失败不致命（处理链语义，
//! 仅记 warnings）。rumi 翻译主线按此取源语言。

use crate::error::Result;
use crate::plugin::{ProcessCapability, RawDoc};

/// 检测采样上限（字符数）：文档更长时只取开头（B 同款量级）。
const MAX_SAMPLE_CHARS: usize = 10_000;
/// 最小样本：低于此不判（whatlang 对极短文本不可靠）。
const MIN_SAMPLE_CHARS: usize = 50;

/// 语言检测处理能力。
pub struct LangDetect;

impl ProcessCapability for LangDetect {
    fn id(&self) -> &str {
        "lang-detect"
    }

    fn should_apply(&self, raw: &RawDoc) -> bool {
        !raw.pages.is_empty() && !raw.meta.contains_key("language")
    }

    fn process(&self, raw: &mut RawDoc) -> Result<()> {
        let sample: String = raw
            .pages
            .iter()
            .flat_map(|p| p.items.iter().map(|i| i.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(MAX_SAMPLE_CHARS)
            .collect();
        if sample.chars().count() < MIN_SAMPLE_CHARS {
            return Ok(()); // 样本不足：不判、不告警（合法静默）
        }
        if let Some(info) = whatlang::detect(&sample) {
            raw.meta.insert("language".to_string(), info.lang().code().to_string());
        }
        Ok(())
    }
}

/// 注册到 Registry 的处理能力位（配合插件 default_process_chain 的
/// "lang-detect" 条目生效）。
pub fn register(reg: &mut crate::registry::Registry) {
    use std::sync::Arc;
    reg.register_process(Arc::new(LangDetect));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{RawPage, RawTextItem};

    fn raw_with(text: &str) -> RawDoc {
        RawDoc {
            pages: vec![RawPage {
                items: vec![RawTextItem { text: text.into(), ..Default::default() }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn chinese_text_detected() {
        let mut raw = raw_with(
            "深秋的校园格外安静，银杏叶铺满了整条林荫道。图书馆的阅览室里，学生们埋头\
             写着各自的报告。会议一直持续到深夜，大家讨论着方案的细节与实施步骤。",
        );
        LangDetect.process(&mut raw).unwrap();
        assert_eq!(raw.meta.get("language").map(String::as_str), Some("cmn"));
    }

    #[test]
    fn short_sample_not_detected() {
        let mut raw = raw_with("太短");
        LangDetect.process(&mut raw).unwrap();
        assert!(!raw.meta.contains_key("language"));
    }

    #[test]
    fn should_apply_skips_existing() {
        let mut raw = raw_with("english text here");
        raw.meta.insert("language".into(), "eng".into());
        assert!(!LangDetect.should_apply(&raw));
    }
}
