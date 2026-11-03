// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① crate 名引用替换；② 组装段在 finalize 前挂
// FormatPlugin::enrich_meta 后置钩子（格式特有元数据 → DocMeta.extra）；
// ③ 无插件报错附 feature 自查提示（fmt_feature_hint 静态映射 + 对应单测）；
// ④ engine 级粗粒度进度钩子（ParsePhase / ProgressFn / with_progress，四阶段
// 边界各通知一次；页级粒度与取消仍在各后端 Options，如 PdfOptions.on_progress）。
//! 文档引擎：sniff → 解析链（回退保留原始错误码）→ 处理链（失败仅记录）→ assemble。
//!
//! 可选引擎级进度钩子（[`ProgressFn`]）在各阶段边界发粗粒度通知，覆盖全部
//! 格式（大 CHM / epub / 压缩包这类黑盒解析的最小观测面）；页级细粒度与
//! 取消由各后端 Options 的回调提供（如 `PdfOptions::on_progress`），两者
//! 并存、互不替代。未挂钩子时零开销（一次判空，无分配）。

use crate::error::{DocError, ErrorCode, Result};
use crate::model::Document;
use crate::policy::BackendPolicy;
use crate::registry::Registry;
use crate::router;

/// 引擎级解析阶段（粗粒度进度通知；页级粒度见各后端 Options 的回调，
/// 如 `PdfOptions::on_progress`——本枚举只标注"走到哪一步"，不含取消语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsePhase {
    /// 嗅探完成（通知体 = 嗅探出的格式名，如 `"txt"`）
    Sniffed,
    /// 解析能力完成（通知体 = 成功产出的解析能力 id，如 `"text.read.txt"`；
    /// 解析链回退时只有最终成功的那一项通知）
    Parsed,
    /// 处理链单项完成（通知体 = 能力 id）。仅通知**实际执行**的单项：
    /// `should_apply` 为 false 被跳过的不通知；`process` 失败也通知
    /// （失败不致命，链继续，错误落 warnings）。
    Processed,
    /// 组装完成（`finalize` 统计回填前发出；无附加标识，通知体为空串）
    Assembled,
}

impl ParsePhase {
    /// 通知体描述符：该阶段回调第二参数承载的标识类型——`Sniffed` 为
    /// 格式名、`Parsed` / `Processed` 为能力 id、`Assembled` 无附加标识
    /// （空串）。实际值随 `parse` 的调用点给出（运行时数据），这里只给
    /// 静态描述，供通用消费者（UI / 日志）解释第二参数的含义。
    pub fn detail(&self) -> &'static str {
        match self {
            ParsePhase::Sniffed => "fmt",
            ParsePhase::Parsed | ParsePhase::Processed => "capability id",
            ParsePhase::Assembled => "",
        }
    }
}

/// 引擎级进度通知（只通知不取消——页级取消在各后端 Options，如
/// `PdfOptions::on_progress` 返回 false 的截停语义）。全同步：实现方
/// 不得在回调里重入同一 engine。
pub type ProgressFn = std::sync::Arc<dyn Fn(ParsePhase, &str) + Send + Sync>;

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

/// 文档引擎：注册表 + 策略 + 可选进度钩子。
pub struct DocumentEngine {
    pub registry: Registry,
    pub policy: BackendPolicy,
    /// 引擎级进度钩子（默认 `None`：零回调、零分配）。
    progress: Option<ProgressFn>,
}

impl DocumentEngine {
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            policy: BackendPolicy::default(),
            progress: None,
        }
    }

    pub fn with_policy(mut self, policy: BackendPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// 挂引擎级进度钩子（builder 风格）。设置后 [`Self::parse`] 在各阶段
    /// 边界回调（见 [`ParsePhase`]）；不设置则全程零回调。页级细粒度与
    /// 取消走各后端 Options 的回调（如 `PdfOptions::on_progress`），与本
    /// 钩子并存：前者回答"走到哪一步"，后者回答"第几页/能否停"。
    pub fn with_progress(mut self, cb: ProgressFn) -> Self {
        self.progress = Some(cb);
        self
    }

    /// 进度分发：未挂钩子时一次判空即返回（无分配、无回调）。
    fn notify(&self, phase: ParsePhase, detail: &str) {
        if let Some(cb) = self.progress.as_ref() {
            cb(phase, detail);
        }
    }

    pub fn parse(&self, path: &str) -> Result<Document> {
        let fmt = router::sniff(path)?;
        // ⓪ 嗅探完成：通知体为格式名（此时插件可能仍缺失——格式已认定）
        self.notify(ParsePhase::Sniffed, fmt);
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
                    // ① 解析链成功：通知体为最终成功的那项能力 id
                    //（回退链里先失败的后端不通知，失败信息走错误返回）
                    self.notify(ParsePhase::Parsed, cap.id());
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
                // ② 单项处理尝试结束即通知（失败也通知——失败不致命，
                // 错误落 warnings，链继续）；被 should_apply 跳过的不通知
                let outcome = proc.process(&mut raw);
                self.notify(ParsePhase::Processed, proc.id());
                if let Err(e) = outcome {
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
        // ③ 组装完成：finalize 统计回填前发出（文档结构已就绪，仅差
        // text_length 等统计字段；无附加标识，通知体空串）
        self.notify(ParsePhase::Assembled, "");
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

    /// 引擎级进度钩子按序通知：txt 件走 parse_with_progress，四阶段边界
    /// 各一条（txt 无处理链 → 无 Processed；lang-detect feature 开启时
    /// default_process_chain 含 "lang-detect"，多一条 Processed）。
    #[test]
    fn progress_phases_reported_in_order() {
        // 长度越过 lang-detect 的最小采样线（MIN_SAMPLE_CHARS = 50），
        // 让 Processed 分支对应一次真实检测而非仅空跑
        let p = write_tmp(
            "prog.txt",
            "docforge 引擎级进度钩子端到端验证：嗅探、解析、处理链与组装四个阶段\
             各在边界发出一条粗粒度通知，文本长度需越过语言检测的最小采样阈值。",
        );
        let phases: std::sync::Arc<std::sync::Mutex<Vec<(ParsePhase, String)>>> = Default::default();
        let sink = phases.clone();
        let cb: ProgressFn = std::sync::Arc::new(move |phase, detail| {
            sink.lock().unwrap().push((phase, detail.to_string()));
        });
        let doc = crate::parse_with_progress(&p, cb).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(doc.meta.file_type, "txt");

        // 阶段序列（cfg 分支：lang-detect 开/关决定是否含 Processed）
        let got: Vec<ParsePhase> = phases.lock().unwrap().iter().map(|(ph, _)| *ph).collect();
        #[cfg(feature = "lang-detect")]
        {
            assert_eq!(
                got,
                [
                    ParsePhase::Sniffed,
                    ParsePhase::Parsed,
                    ParsePhase::Processed,
                    ParsePhase::Assembled
                ]
            );
        }
        #[cfg(not(feature = "lang-detect"))]
        {
            assert_eq!(
                got,
                [ParsePhase::Sniffed, ParsePhase::Parsed, ParsePhase::Assembled]
            );
        }

        // 通知体：Sniffed = 格式名，Parsed = 成功解析能力 id，
        // Processed（lang-detect）= 能力 id，Assembled = 空串
        let collected = phases.lock().unwrap();
        let details: Vec<&str> = collected.iter().map(|(_, d)| d.as_str()).collect();
        assert_eq!(details[0], "txt");
        assert_eq!(details[1], "text.read.txt");
        #[cfg(feature = "lang-detect")]
        assert_eq!(details[2], "lang-detect");
        assert_eq!(details[details.len() - 1], "");
    }

    /// 默认路径零回调：`parse()` 便捷函数与 `DocumentEngine::new` 均不挂钩子
    /// （progress 字段为 None），全程通知计数保持 0。
    #[test]
    fn progress_disabled_by_default() {
        let p = write_tmp("prog_off.txt", "hello\n");
        // 已武装的计数钩子：不被任何默认装配持有——若默认路径错误触发了
        // 通知，计数立即非零（含未来引入环境级/线程局部钩子的回归防护）
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = count.clone();
        let _armed: ProgressFn = std::sync::Arc::new(move |_phase, _detail| {
            sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        // 便捷函数 parse()：自建默认引擎（无钩子）
        let doc = crate::parse(&p).unwrap();
        assert_eq!(doc.meta.file_type, "txt");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);

        // 构造路径：builder 未调 with_progress 时字段保持 None
        let eng = engine();
        assert!(eng.progress.is_none());
        assert!(eng.parse(&p).is_ok());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        std::fs::remove_file(&p).ok();
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
