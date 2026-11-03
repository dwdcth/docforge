// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! 文档引擎：sniff → 解析链（回退保留原始错误码）→ 处理链（失败仅记录）→ assemble。

use crate::error::{DocError, ErrorCode, Result};
use crate::model::Document;
use crate::policy::BackendPolicy;
use crate::registry::Registry;
use crate::router;

/// 文档引擎：注册表 + 策略。
pub struct DocumentEngine {
    pub registry: Registry,
    pub policy: BackendPolicy,
}

impl DocumentEngine {
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            policy: BackendPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: BackendPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn parse(&self, path: &str) -> Result<Document> {
        let fmt = router::sniff(path)?;
        let plugin = self
            .registry
            .plugin_for(fmt)
            .ok_or_else(|| DocError::unsupported(format!("没有可处理该格式的插件: {fmt}")))?;

        // ① 解析链：按序尝试，失败保留原始已分类错误（不吞成 corrupt）
        let caps =
            self.registry
                .resolve_parse_chain(fmt, &self.policy, &plugin.default_parse_chain());
        let mut raw = None;
        let mut last_err: Option<DocError> = None;
        for cap in caps {
            match cap.parse(path) {
                Ok(r) => {
                    raw = Some(r);
                    break;
                }
                Err(e) => {
                    // 终止码不回退：encrypted/unsupported 是确定性结论，
                    // 不能被兜底后端（如 v1 对加密件的 corrupt）掩盖
                    if matches!(e.code, ErrorCode::Encrypted | ErrorCode::Unsupported) {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        let mut raw = raw.ok_or_else(|| {
            last_err.unwrap_or_else(|| DocError::corrupt(format!("全部解析后端失败: {fmt}")))
        })?;

        // ② 处理链：就地增强（失败不致命，仅记录）
        let proc_chain = self
            .registry
            .resolve_process_chain(&self.policy, &plugin.default_process_chain());
        let mut warnings = Vec::new();
        for proc in proc_chain {
            if proc.should_apply(&raw) {
                if let Err(e) = proc.process(&mut raw) {
                    warnings.push(format!("{}: {e}", proc.id()));
                }
            }
        }
        if !warnings.is_empty() {
            raw.meta.insert("warnings".into(), warnings.join("; "));
        }

        // ③ 组装 IR
        let mut doc = plugin.assemble(&raw);
        doc.meta.file_type = fmt.to_string();
        // 处理链告警此前只留在 RawDoc.meta 里（随 raw 丢弃），落到 doc.meta.extra 才可见
        if let Some(w) = raw.meta.get("warnings") {
            doc.meta.extra.insert("warnings".to_string(), w.clone());
        }
        Ok(doc.finalize())
    }
}

// 导出方便错误码检查（示例用）
#[allow(dead_code)]
fn _code(e: &DocError) -> ErrorCode {
    e.code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::text::register_defaults;

    fn engine() -> DocumentEngine {
        let mut reg = Registry::new();
        register_defaults(&mut reg);
        DocumentEngine::new(reg)
    }

    fn write_tmp(name: &str, content: &str) -> String {
        let p = std::env::temp_dir().join(format!("docforge_engine_{name}"));
        std::fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn parse_txt_end_to_end() {
        let p = write_tmp("a.txt", "line1\n\nline2\n");
        let doc = engine().parse(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(doc.meta.file_type, "txt");
        assert_eq!(doc.meta.text_length, 12); // "line1\n\nline2"
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].text, "line1\n\nline2");
        assert_eq!(doc.pages[0].blocks.len(), 2); // 空行不作为块
    }

    #[test]
    fn sniffed_unsupported_raises_code() {
        let mut reg = Registry::new();
        register_defaults(&mut reg);
        let eng = DocumentEngine::new(reg);
        let p = write_tmp("fake.doc", "not an ole file\n");
        let err = eng.parse(&p).unwrap_err();
        std::fs::remove_file(&p).ok();
        assert_eq!(err.code, ErrorCode::Unsupported);
    }

    #[test]
    #[cfg(feature = "md")]
    fn parse_markdown_end_to_end() {
        let p = write_tmp("a.md", "# 标题\n\n- 甲\n- 乙\n\n```py\nprint(1)\n```\n");
        let doc = engine().parse(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(doc.meta.file_type, "md");
        let kinds: Vec<crate::model::BlockKind> =
            doc.pages[0].blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                crate::model::BlockKind::Heading,
                crate::model::BlockKind::List,
                crate::model::BlockKind::Code
            ]
        );
        assert_eq!(doc.pages[0].blocks[0].level, Some(1));
        assert_eq!(doc.pages[0].blocks[1].ordered, Some(false));
    }

    #[test]
    #[cfg(feature = "csv")]
    fn parse_csv_end_to_end() {
        let p = write_tmp("a.csv", "h1,h2\n\"a,1\",2\n");
        let doc = engine().parse(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(doc.meta.file_type, "csv");
        let table = doc.pages[0].blocks[0].table.as_ref().unwrap();
        assert_eq!(table.rows[1], vec!["a,1", "2"]);
    }

    #[test]
    #[cfg(feature = "html")]
    fn parse_html_end_to_end() {
        let p = write_tmp(
            "a.html",
            "<h2>小节</h2><p>正文</p><table><tr><td>x</td></tr></table>",
        );
        let doc = engine().parse(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(doc.meta.file_type, "html");
        let kinds: Vec<crate::model::BlockKind> =
            doc.pages[0].blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                crate::model::BlockKind::Heading,
                crate::model::BlockKind::Paragraph,
                crate::model::BlockKind::Table
            ]
        );
        assert_eq!(doc.pages[0].blocks[0].level, Some(2));
    }
}
