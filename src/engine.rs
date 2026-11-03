// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① crate 名引用替换；② 组装段在 finalize 前挂
// FormatPlugin::enrich_meta 后置钩子（格式特有元数据 → DocMeta.extra）；
// ③ 无插件报错附 feature 自查提示（fmt_feature_hint 静态映射 + 对应单测）。
//! 文档引擎：sniff → 解析链（回退保留原始错误码）→ 处理链（失败仅记录）→ assemble。

use crate::error::{DocError, ErrorCode, Result};
use crate::model::Document;
use crate::policy::BackendPolicy;
use crate::registry::Registry;
use crate::router;

/// 格式名 → feature 自查提示：`没有可处理该格式的插件` 报错时的指路映射。
///
/// 与 [`crate::router::sniff`] 产出的格式名一一对应（静态表，不依赖编译开关——
/// 提示的正是"当前编译缺哪个 feature"，故映射必须无条件可用）：
/// - 同名 feature：docx / xlsx / pptx / pdf / djvu / epub / fb2 / chm / cbz；
/// - 家族 feature：odt/ods/odp → `odf`，mobi/prc/azw3 → `mobi`，
///   图像直读 → `image-files`，压缩包 → `archive`；
/// - txt/md/csv/html/rtf/字幕（srt/vtt/ass/ssa）属 default 面，返回 `None`：
///   报这个错说明 default-features 被裁了，指路 default-features 而非加 feature。
fn fmt_feature_hint(fmt: &str) -> Option<&'static str> {
    match fmt {
        "docx" => Some("docx"),
        "xlsx" => Some("xlsx"),
        "pptx" => Some("pptx"),
        "pdf" => Some("pdf"),
        "djvu" => Some("djvu"),
        "epub" => Some("epub"),
        "fb2" => Some("fb2"),
        "chm" => Some("chm"),
        "cbz" => Some("cbz"),
        "odt" | "ods" | "odp" => Some("odf"),
        "mobi" | "prc" | "azw3" => Some("mobi"),
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff" => Some("image-files"),
        "zip" | "rar" | "7z" => Some("archive"),
        _ => None,
    }
}

/// 无插件错误的最终文案：feature 类指路 Cargo.toml，default 类指路
/// default-features（错误码保持 Unsupported 不变，只加自查提示后缀）。
fn no_plugin_message(fmt: &str) -> String {
    let hint = match fmt_feature_hint(fmt) {
        Some(feat) => {
            format!("（在 Cargo.toml 的 docforge 依赖加 features = [\"{feat}\"] 后重新编译）")
        }
        None => "（当前编译未含文本族，检查 default-features 设置）".to_string(),
    };
    format!("没有可处理该格式的插件: {fmt}{hint}")
}

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
            .ok_or_else(|| DocError::unsupported(no_plugin_message(fmt)))?;

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

        // ③ 组装 IR（后置钩子：插件把 frontmatter / 书籍元数据写入 extra；
        // 必须在 finalize 统计回填之前完成）
        let mut doc = plugin.assemble(&raw);
        plugin.enrich_meta(&raw, &mut doc);
        doc.meta.file_type = fmt.to_string();
        // 处理链告警此前只留在 RawDoc.meta 里（随 raw 丢弃），落到 doc.meta.extra 才可见
        if let Some(w) = raw.meta.get("warnings") {
            doc.meta.extra.insert("warnings".to_string(), w.clone());
        }
        // 语言检测结论（lang-detect 处理能力写入）同样搬到 extra（docforge M2）
        if let Some(lang) = raw.meta.get("language") {
            doc.meta.extra.insert("language".to_string(), lang.clone());
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

    /// 无插件报错的 feature 指路：映射逐键断言 + 引擎侧两条路径的最终文案。
    /// 全部走手工构造的空 registry，不依赖任何 feature 开关（任意组合下稳定）。
    #[test]
    fn error_message_hints_feature() {
        // 静态映射：同名 feature / 家族 feature / default 面与未知名 → None
        for (fmt, want) in [
            ("docx", Some("docx")),
            ("xlsx", Some("xlsx")),
            ("pptx", Some("pptx")),
            ("pdf", Some("pdf")),
            ("djvu", Some("djvu")),
            ("epub", Some("epub")),
            ("fb2", Some("fb2")),
            ("chm", Some("chm")),
            ("cbz", Some("cbz")),
            ("odt", Some("odf")),
            ("ods", Some("odf")),
            ("odp", Some("odf")),
            ("mobi", Some("mobi")),
            ("prc", Some("mobi")),
            ("azw3", Some("mobi")),
            ("png", Some("image-files")),
            ("jpg", Some("image-files")),
            ("jpeg", Some("image-files")),
            ("webp", Some("image-files")),
            ("bmp", Some("image-files")),
            ("tif", Some("image-files")),
            ("tiff", Some("image-files")),
            ("zip", Some("archive")),
            ("rar", Some("archive")),
            ("7z", Some("archive")),
            ("txt", None),
            ("md", None),
            ("csv", None),
            ("html", None),
            ("rtf", None),
            ("srt", None),
            ("vtt", None),
            ("ass", None),
            ("ssa", None),
        ] {
            assert_eq!(fmt_feature_hint(fmt), want, "{fmt}");
        }

        // 引擎侧 default 路径（txt）：文案指路 default-features
        let p = write_tmp("hint_a.txt", "hello\n");
        let err = DocumentEngine::new(Registry::new()).parse(&p).unwrap_err();
        std::fs::remove_file(&p).ok();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert_eq!(
            err.message,
            "没有可处理该格式的插件: txt（当前编译未含文本族，检查 default-features 设置）"
        );

        // 引擎侧 feature 路径（docx）：文案指路 Cargo.toml 加 feature
        //（空 registry 未注册 office 插件，docx feature 开关无关，文案稳定）
        let p = write_tmp("hint_b.docx", "PK\x03\x04-truncated");
        let err = DocumentEngine::new(Registry::new()).parse(&p).unwrap_err();
        std::fs::remove_file(&p).ok();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert_eq!(
            err.message,
            "没有可处理该格式的插件: docx（在 Cargo.toml 的 docforge 依赖加 features = [\"docx\"] 后重新编译）"
        );
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
