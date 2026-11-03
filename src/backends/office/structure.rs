// 吸收自 docparser-office 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写（docparser_core→docforge、office 内部→super）。
//! `structure_mode` 三态（对齐 Python `ooxml._semantic`）：
//!
//! * `raw` —— 只出文本：不做标题/列表识别、不做几何排序、表格压平成一段文本、
//!   pptx 备注整体丢弃；页文本 = 各行文本按 `\n` 拼接。assemble 阶段**按行产
//!   Paragraph 块**（docforge 刻意偏离 Python 的 `blocks=[]`：零块文档对翻译/
//!   RAG 消费者等于不可用，见 office/mod.rs 的 flat 分支注释）。
//! * `semantic` —— 全结构（历史行为）。
//! * `auto` —— 按 cheap probe 路由：docx 看 `document.xml` 原始字节里的结构信号，
//!   pptx 按**每张幻灯片**自己的形状数/表格/图表判定。
//!
//! probe 判据必须与 Python 逐字一致，否则 `office_parity.py --structure-mode auto`
//! 会在个别文件上分叉（一个走快路径、一个走全结构）。

use std::str::FromStr;

/// 结构还原三态（对应 Python `policy.structure_mode`，默认 `auto`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StructureMode {
    /// 按 cheap probe 结果路由（默认）
    #[default]
    Auto,
    /// 只出文本，不做任何结构还原
    Raw,
    /// 全结构还原
    Semantic,
}

impl StructureMode {
    /// 三态 → 是否做结构还原。`has_structure` 是调用方的 cheap probe 结果，
    /// 只有 `Auto` 会用到它（对齐 Python `_semantic(ctx, default)`）。
    pub fn resolve(self, has_structure: bool) -> bool {
        match self {
            StructureMode::Raw => false,
            StructureMode::Semantic => true,
            StructureMode::Auto => has_structure,
        }
    }

    /// 策略名（与 Python `policy.structure_mode` 取值一致，用于 CLI/日志）
    pub fn as_str(self) -> &'static str {
        match self {
            StructureMode::Auto => "auto",
            StructureMode::Raw => "raw",
            StructureMode::Semantic => "semantic",
        }
    }
}

impl FromStr for StructureMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(StructureMode::Auto),
            "raw" => Ok(StructureMode::Raw),
            "semantic" => Ok(StructureMode::Semantic),
            other => Err(format!(
                "unknown structure_mode `{other}`（可选 auto / raw / semantic）"
            )),
        }
    }
}

/// docx cheap probe（对齐 Python `DocxParseCapability.parse` 里的字节判定）：
/// 有表格 / 编号 / 大纲级 / 嵌入图 / `Heading*` 样式 / 结构化文档控件 / 中文「标题」字样
/// 就认为值得做结构还原。判据用 `str::contains`，与 Python 的 `bytes in data` 等价
/// （`document.xml` 是 UTF-8）。
pub fn docx_has_structure(xml: &str) -> bool {
    const MARKERS: [&str; 7] = [
        "<w:tbl",
        "numPr",
        "outlineLvl",
        "<w:drawing",
        "Heading",
        "<w:sdt",
        "标题",
    ];
    MARKERS.iter().any(|m| xml.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_three_states() {
        assert!(!StructureMode::Raw.resolve(true));
        assert!(StructureMode::Semantic.resolve(false));
        assert!(StructureMode::Auto.resolve(true));
        assert!(!StructureMode::Auto.resolve(false));
    }

    #[test]
    fn parse_names() {
        assert_eq!("raw".parse::<StructureMode>().unwrap(), StructureMode::Raw);
        assert_eq!(
            "SEMANTIC".parse::<StructureMode>().unwrap(),
            StructureMode::Semantic
        );
        assert_eq!("".parse::<StructureMode>().unwrap(), StructureMode::Auto);
        assert!("fast".parse::<StructureMode>().is_err());
        assert_eq!(StructureMode::default().as_str(), "auto");
    }

    #[test]
    fn docx_probe_markers() {
        assert!(docx_has_structure("<w:body><w:tbl>"));
        assert!(docx_has_structure(r#"<w:numPr><w:ilvl w:val="0"/>"#));
        assert!(docx_has_structure(r#"<w:pStyle w:val="Heading1"/>"#));
        assert!(docx_has_structure("<w:sdt>"));
        assert!(docx_has_structure("<w:p><w:r><w:t>标题</w:t></w:r></w:p>"));
        // 纯文本段落：无任何结构信号 → auto 走 raw
        assert!(!docx_has_structure(
            "<w:body><w:p><w:r><w:t>甲乙丙</w:t></w:r></w:p></w:body>"
        ));
    }
}
