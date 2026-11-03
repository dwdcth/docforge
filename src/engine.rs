// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：① crate 名引用替换；② 组装段在 finalize 前挂
// FormatPlugin::enrich_meta 后置钩子（格式特有元数据 → DocMeta.extra）；
// ③ 无插件报错附 feature 自查提示（fmt_feature_hint 静态映射 + 对应单测）；
// ④ engine 级粗粒度进度钩子（ParsePhase / ProgressFn / with_progress，四阶段
// 边界各通知一次；页级粒度与取消仍在各后端 Options，如 PdfOptions.on_progress）；
// ⑤ engine 级流式解析（PageSink / parse_streaming：assemble 后逐页投递，统计
// 按已投递页回填、warnings 记中止；pdf 千页件的最坏场景另有
// backends::pdf::parse_streaming 深度流式入口，页产即吐、峰值 = 单页）；
// ⑥ 块级语言检测后置遍历（policy.lang_detect_per_block × feature lang-detect
// 双闸：assemble 后、finalize/页投递前逐块填 Block.lang，见 detect_block_langs）。
//! 文档引擎：sniff → 解析链（回退保留原始错误码）→ 处理链（失败仅记录）→ assemble。
//!
//! 可选引擎级进度钩子（[`ProgressFn`]）在各阶段边界发粗粒度通知，覆盖全部
//! 格式（大 CHM / epub / 压缩包这类黑盒解析的最小观测面）；页级细粒度与
//! 取消由各后端 Options 的回调提供（如 `PdfOptions::on_progress`），两者
//! 并存、互不替代。未挂钩子时零开销（一次判空，无分配）。

use crate::error::{DocError, ErrorCode, Result};
use crate::model::{DocMeta, Document, Page};
use crate::plugin::{FormatPlugin, RawDoc};
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

/// 页接收器：流式解析逐页投递（见 [`DocumentEngine::parse_streaming`]）。
/// 返回 false 中止（余页不产，meta 仍返回，warnings 记中止）。消费即释放
/// ——调用方不应长期持有 [`Page`]。
pub type PageSink<'a> = &'a mut dyn FnMut(Page) -> bool;

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
        let (fmt, plugin, raw) = self.run_parse_chain(path)?;
        // mut 随 feature：块级语言检测（仅 lang-detect 有检测函数）需要就地
        // 改块；无该 feature 时 finalize 按值消费，绑定无需 mut（避免
        // unused_mut 告警）
        #[cfg(feature = "lang-detect")]
        let mut doc = Self::assemble_document(fmt, plugin.as_ref(), &raw);
        #[cfg(not(feature = "lang-detect"))]
        let doc = Self::assemble_document(fmt, plugin.as_ref(), &raw);
        // ③ 组装完成：finalize 统计回填前发出（文档结构已就绪，仅差
        // text_length 等统计字段；无附加标识，通知体空串）
        self.notify(ParsePhase::Assembled, "");
        // ③⁺ 块级语言检测（policy 开关 × feature 双闸）：放 Assembled 通知
        // 之后、finalize 之前——Assembled 语义是"结构就绪"，不含语言填充，
        // 先后皆可；落 finalize 前保证返回的 Document 已带块级 lang。
        #[cfg(feature = "lang-detect")]
        if self.policy.lang_detect_per_block {
            detect_block_langs(&mut doc);
        }
        Ok(doc.finalize())
    }

    /// 共享前段（⓪①②）：sniff → 插件 → 解析链（回退保留原始错误码）→
    /// 处理链（就地增强，失败仅记录告警并回填 RawDoc.meta）。[`Self::parse`]
    /// 与 [`Self::parse_streaming`] 共用；进度通知点与旧实现一一对应
    ///（Sniffed / Parsed / Processed 在此发出，Assembled 留给组装段之后）。
    fn run_parse_chain(
        &self,
        path: &str,
    ) -> Result<(&'static str, std::sync::Arc<dyn FormatPlugin>, RawDoc)> {
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
        Ok((fmt, plugin, raw))
    }

    /// ③ 组装段（finalize 之前）：assemble + enrich_meta 后置钩子 +
    /// file_type 回填 + warnings/language 搬运 extra。parse 与
    /// parse_streaming 共用；返回的 Document 尚未回填统计（finalize 由
    /// 调用方择机执行）。
    fn assemble_document(fmt: &str, plugin: &dyn FormatPlugin, raw: &RawDoc) -> Document {
        let mut doc = plugin.assemble(raw);
        plugin.enrich_meta(raw, &mut doc);
        doc.meta.file_type = fmt.to_string();
        // 处理链告警此前只留在 RawDoc.meta 里（随 raw 丢弃），落到 doc.meta.extra 才可见
        if let Some(w) = raw.meta.get("warnings") {
            doc.meta.extra.insert("warnings".to_string(), w.clone());
        }
        // 语言检测结论（lang-detect 处理能力写入）同样搬到 extra（docforge M2）
        if let Some(lang) = raw.meta.get("language") {
            doc.meta.extra.insert("language".to_string(), lang.clone());
        }
        doc
    }

    /// 流式解析：assemble 后逐页喂 sink（不攒 Document），返回 DocMeta
    ///（统计按已投递页回填）。RawDoc 阶段仍全量（解析器现实），峰值内存 =
    /// RawDoc + 单页（对比 [`Self::parse`] 的 RawDoc + Document 双驻留减半：
    /// 处理链结束即释放 RawDoc，组装页投递后即弃）。
    ///
    /// # 语义
    /// - 投递序 = 页序；sink 返回 false 即中止：余页不产不投，DocMeta 仍
    ///   返回（统计 = 已投递页集合——被拒页已送达 sink，计为已投递），
    ///   warnings 附"流式消费中止于第 N 页"。进度钩子照常通知（四阶段边界
    ///   与 [`Self::parse`] 完全一致）。
    /// - 统计回填复用 [`Document::finalize`]：构造只含已投递页的临时
    ///   Document 再 finalize。已投递页在引擎侧只留"统计幽灵"（finalize
    ///   实际读取的 text / images.len / ocr_text），块结构与图像字节随投递
    ///   释放，不在引擎侧长期驻留。
    /// - 消费即释放——调用方不应长期持有 Page。
    /// - pdf 千页扫描件（keep_raster_pages 全量驻留爆内存）的最坏场景请
    ///   直接用深度流式入口 `crate::backends::pdf::parse_streaming`：绕过
    ///   RawDoc/assemble 全量驻留，页产即吐、峰值 = 单页。本方法对 pdf
    ///   同样正确，只是受插件解析器现实所限省不掉 RawDoc 阶段。
    pub fn parse_streaming(&self, path: &str, sink: PageSink<'_>) -> Result<DocMeta> {
        let (fmt, plugin, raw) = self.run_parse_chain(path)?;
        let mut doc = Self::assemble_document(fmt, plugin.as_ref(), &raw);
        drop(raw); // 流式：处理链已结束，RawDoc 先于页投递释放（不再双驻留）
        // ③ 组装完成：通知点与 parse 一致（finalize 统计回填前发出）
        self.notify(ParsePhase::Assembled, "");
        // ③⁺ 块级语言检测（policy 开关 × feature 双闸）：必须在页投递之前
        // ——mem::take 把页交出去后再填，块级 lang 就随引擎侧副本丢了，
        // sink 收到的页永远不带检测结论。
        #[cfg(feature = "lang-detect")]
        if self.policy.lang_detect_per_block {
            detect_block_langs(&mut doc);
        }

        // ④ 逐页投递：mem::take 消费组装页，sink false 即中止（余页不产）。
        // 统计幽灵先于投递构建（只克隆 finalize 读取的字段），载荷随 Page
        // 交给消费方。
        let pages = std::mem::take(&mut doc.pages);
        let total = pages.len();
        let mut ghosts: Vec<Page> = Vec::with_capacity(total);
        let mut aborted_at: Option<usize> = None;
        for (i, page) in pages.into_iter().enumerate() {
            ghosts.push(ghost_page(&page));
            if !sink(page) {
                aborted_at = Some(i + 1);
                break;
            }
        }

        // ⑤ meta 统计按已投递页回填：构造只含已投递页的临时 Document 复用
        // finalize 统计逻辑；中止标记附进 warnings（与处理链告警同 "; " 连接）
        let mut meta_doc = Document { meta: std::mem::take(&mut doc.meta), pages: ghosts };
        if let Some(n) = aborted_at {
            append_warning(&mut meta_doc.meta.extra, format!("流式消费中止于第 {n} 页"));
        }
        Ok(meta_doc.finalize().meta)
    }
}

/// 统计幽灵页：只保留 [`Document::finalize`] 实际读取的字段（text / images
/// 的条数与 ocr_text）。块结构与图像字节不驻留——流式投递后载荷随 Page
/// 交给消费方，引擎侧只留统计骨架。engine 层与 pdf 深度流式共用同一口径。
pub(crate) fn ghost_page(page: &Page) -> Page {
    Page {
        text: page.text.clone(),
        images: page
            .images
            .iter()
            .map(|im| crate::model::Image {
                ocr_text: im.ocr_text.clone(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// warnings 追加（与处理链告警的 "; " join 语义一致；无则新建条目）。
fn append_warning(extra: &mut std::collections::BTreeMap<String, String>, msg: String) {
    match extra.get_mut("warnings") {
        Some(w) => {
            w.push_str("; ");
            w.push_str(&msg);
        }
        None => {
            extra.insert("warnings".to_string(), msg);
        }
    }
}

/// 块级语言检测的样本下限（chars 计）。whatlang 对短文本不可靠：文档级
/// `lang_detect.rs` 的 MIN_SAMPLE_CHARS = 50 面向的是跨页段落聚合样本
/// （天然更长），块级单块普遍更短，取 20 折中——够滤掉标题/短句噪声，
/// 又不至于让普通段落的块全部哑火。
#[cfg(feature = "lang-detect")]
const BLOCK_MIN_SAMPLE_CHARS: usize = 20;

/// 块级语言检测（feature = "lang-detect"）：给可检测且未标语言的块填
/// `Block.lang`（ISO 639-3 码，与文档级 `plugins/lang_detect.rs` 同码制）。
///
/// 为什么走 engine 组装后置遍历、而非 RawTextItem 透传：ProcessCapability
/// 按 trait 设计工作在 RawDoc（组装前）层，若在 item 层填 lang，15+ 个解析
/// 后端的 assemble 都要逐个透传 `RawTextItem.lang → Block.lang`，散且漏。
/// 块级检测的输入就是块文本，engine 在 assemble 之后单点遍历一次，全部
/// 格式一处覆盖。
///
/// 样本口径：
/// - 普通块用 `content.trim()`；Table 块的 content 只是表头摘要，改用
///   `table.rows` 的非空格单元格 `join("\n")` 作样本；
/// - 样本 < [`BLOCK_MIN_SAMPLE_CHARS`] 的块跳过（理由见常量注释）；
/// - 已有 `lang` 的块不覆盖（尊重后端自带结论），`whatlang::detect` 未
///   命中同样保持 None（静默，合法）。
#[cfg(feature = "lang-detect")]
fn detect_block_langs(doc: &mut Document) {
    for page in &mut doc.pages {
        for block in &mut page.blocks {
            if block.lang.is_some() {
                continue;
            }
            let sample = match block.table.as_ref() {
                Some(t) => t
                    .rows
                    .iter()
                    .flat_map(|row| row.iter().map(|cell| cell.trim()))
                    .filter(|cell| !cell.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => block.content.trim().to_string(),
            };
            if sample.chars().count() < BLOCK_MIN_SAMPLE_CHARS {
                continue;
            }
            if let Some(info) = whatlang::detect(&sample) {
                block.lang = Some(info.lang().code().to_string());
            }
        }
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

    /// 带 policy 的引擎（块级语言检测开关经此传入）。
    #[cfg(feature = "lang-detect")]
    fn engine_with_policy(policy: BackendPolicy) -> DocumentEngine {
        let mut reg = Registry::new();
        register_defaults(&mut reg);
        DocumentEngine::new(reg).with_policy(policy)
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

    /// 流式投递：sink 收页序 = 页序、页文本与全量 parse 一致、meta 统计
    /// 与全量一致。txt / md 件各验一遍（spec 件型）；另以 srt 字幕件
    ///（每条 cue 一页，始终编译）真验多页投递序。
    #[test]
    fn streaming_sink_receives_pages_in_order() {
        // txt：单页，文本 / 统计与全量 parse 对齐
        let p = write_tmp("stream_a.txt", "line1\n\nline2\n");
        let mut got: Vec<Page> = Vec::new();
        let meta = engine()
            .parse_streaming(&p, &mut |page: Page| {
                got.push(page);
                true
            })
            .unwrap();
        let full = engine().parse(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "line1\n\nline2");
        assert_eq!(got[0].blocks.len(), 2);
        assert_eq!(meta.file_type, "txt");
        assert_eq!(meta.page_count, full.pages.len());
        assert_eq!(meta.text_length, full.meta.text_length);
        assert_eq!(meta.image_count, 0);

        // md：块结构照常进 IR（块级语法解析不受流式影响）
        #[cfg(feature = "md")]
        {
            let mp = write_tmp("stream.md", "# 标题\n\n正文一段\n");
            let mut mpages: Vec<Page> = Vec::new();
            let mmeta = engine()
                .parse_streaming(&mp, &mut |page: Page| {
                    mpages.push(page);
                    true
                })
                .unwrap();
            std::fs::remove_file(&mp).ok();
            assert_eq!(mmeta.file_type, "md");
            assert_eq!(mpages.len(), 1);
            assert_eq!(mpages[0].blocks[0].kind, crate::model::BlockKind::Heading);
            assert_eq!(mpages[0].blocks[0].level, Some(1));
        }

        // srt 多页：投递序 = cue 序（页 name / 文本逐页对齐全量 parse）
        let sp = write_tmp(
            "stream.srt",
            "1\n00:00:01,000 --> 00:00:02,000\n第一句\n\n2\n00:00:03,000 --> 00:00:04,000\n第二句\n",
        );
        let mut spages: Vec<Page> = Vec::new();
        let smeta = engine()
            .parse_streaming(&sp, &mut |page: Page| {
                spages.push(page);
                true
            })
            .unwrap();
        let sfull = engine().parse(&sp).unwrap();
        std::fs::remove_file(&sp).ok();
        assert!(
            spages.len() >= 2,
            "srt 件应产多页（实际 {} 页），多页投递序才有断言力",
            spages.len()
        );
        let got_names: Vec<_> = spages.iter().map(|pg| pg.name.clone()).collect();
        let want_names: Vec<_> = sfull.pages.iter().map(|pg| pg.name.clone()).collect();
        assert_eq!(got_names, want_names, "投递序 = 页序");
        for (i, (gp, fp)) in spages.iter().zip(sfull.pages.iter()).enumerate() {
            assert_eq!(gp.text, fp.text, "第 {} 页文本与全量不一致", i + 1);
        }
        assert_eq!(smeta.page_count, sfull.pages.len());
        assert_eq!(smeta.text_length, sfull.meta.text_length);
        assert_eq!(smeta.image_count, 0);
        assert_eq!(smeta.file_type, "srt");
    }

    /// 流式中止：第 1 页 sink false → 恰投递 1 页、meta.page_count == 1
    ///（被拒页已送达 sink，计为已投递）、warnings 含中止标记、统计按已
    /// 投递页回填。
    #[test]
    fn streaming_cancel_stops_delivery() {
        let p = write_tmp("stream_cancel.txt", "第一页内容\n");
        let mut delivered = 0usize;
        let meta = engine()
            .parse_streaming(&p, &mut |_page: Page| {
                delivered += 1;
                false
            })
            .unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(delivered, 1, "sink false 即停，余页不投");
        assert_eq!(meta.page_count, 1);
        assert_eq!(meta.text_length, "第一页内容".chars().count());
        let w = meta.extra.get("warnings").expect("应有中止告警");
        assert!(w.contains("流式消费中止于第 1 页"), "实际告警：{w}");
    }

    // ------------------------------------------------------------------
    // 块级语言检测（policy.lang_detect_per_block × feature lang-detect）
    // ------------------------------------------------------------------

    /// 中英混排 txt 的块级语言检测：开关开 → 两块分别 cmn / eng（逐块
    /// 走不同翻译方向的钥匙）；开关关（默认）→ 全部 None。两层检测并存
    /// 不冲突：文档级 `meta.extra["language"]` 在两种开关状态下照旧存在。
    #[test]
    #[cfg(feature = "lang-detect")]
    fn per_block_lang_detect_mixed_txt() {
        let cn = "深秋的校园格外安静，银杏叶铺满了整条林荫道，图书馆的阅览室里学生们埋头整理着各自的手稿与读书笔记。";
        let en = "The rendering pipeline assembles each frame from a queue of command buffers before the final image is presented to the display.";
        let p = write_tmp("mixlang.txt", &format!("{cn}\n\n{en}\n"));

        let on = engine_with_policy(BackendPolicy {
            lang_detect_per_block: true,
            ..Default::default()
        })
        .parse(&p)
        .unwrap();
        let off = engine().parse(&p).unwrap();
        std::fs::remove_file(&p).ok();

        let blocks = &on.pages[0].blocks;
        assert_eq!(blocks.len(), 2, "空行分段：中文段 + 英文段各一块");
        assert_eq!(blocks[0].lang.as_deref(), Some("cmn"), "中文块");
        assert_eq!(blocks[1].lang.as_deref(), Some("eng"), "英文块");

        // 开关关（默认 false）：块 lang 保持 None（feature 在、策略不开）
        assert!(
            off.pages[0].blocks.iter().all(|b| b.lang.is_none()),
            "per_block 关闭时不得填块级 lang"
        );

        // 文档级检测不受块级开关影响：主导语言照旧写 extra（两层并存）
        assert!(on.meta.extra.contains_key("language"));
        assert!(off.meta.extra.contains_key("language"));
    }

    /// 流式路径同验：sink 收到的页上块 lang 已填——证明调用点在
    /// `mem::take` 页投递之前（页交出去后再填就随引擎侧副本丢了）。
    #[test]
    #[cfg(feature = "lang-detect")]
    fn per_block_lang_detect_streaming_filled_before_take() {
        let cn = "深秋的校园格外安静，银杏叶铺满了整条林荫道，图书馆的阅览室里学生们埋头整理着各自的手稿与读书笔记。";
        let en = "The rendering pipeline assembles each frame from a queue of command buffers before the final image is presented to the display.";
        let p = write_tmp("mixlang_stream.txt", &format!("{cn}\n\n{en}\n"));
        let mut got: Vec<Page> = Vec::new();
        let meta = engine_with_policy(BackendPolicy {
            lang_detect_per_block: true,
            ..Default::default()
        })
        .parse_streaming(
            &p,
            &mut |page: Page| {
                got.push(page);
                true
            },
        )
        .unwrap();
        std::fs::remove_file(&p).ok();

        assert_eq!(got.len(), 1);
        let blocks = &got[0].blocks;
        assert_eq!(blocks[0].lang.as_deref(), Some("cmn"));
        assert_eq!(blocks[1].lang.as_deref(), Some("eng"));
        // 文档级结论照旧随 meta 返回
        assert!(meta.extra.contains_key("language"));
    }

    /// 短块阈值：样本 < 20 chars 的块即使开关打开也不判（whatlang 短文本
    /// 不可靠）；同文档里的长块照常判出——阈值按块独立，不按文档。
    #[test]
    #[cfg(feature = "lang-detect")]
    fn per_block_lang_skips_short_blocks() {
        let p = write_tmp(
            "shortlang.txt",
            "短说明。\n\nThe rendering pipeline assembles each frame from a queue of command buffers before presenting.\n",
        );
        let doc = engine_with_policy(BackendPolicy {
            lang_detect_per_block: true,
            ..Default::default()
        })
        .parse(&p)
        .unwrap();
        std::fs::remove_file(&p).ok();

        let blocks = &doc.pages[0].blocks;
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].lang.is_none(), "短块（4 chars < 20）不判");
        assert_eq!(blocks[1].lang.as_deref(), Some("eng"), "同文档长块照常判出");
    }

    /// Table 块样本口径：content 只是表头摘要（这里刻意 < 20 字符，若误用
    /// content 作样本将保持 None），实际样本取 `table.rows` 非空格单元格
    /// join → 判出全中文表格的语言。直接喂手造 Document 验检测函数本体，
    /// 不依赖任何格式 feature。
    #[test]
    #[cfg(feature = "lang-detect")]
    fn table_block_sample_uses_rows_not_content() {
        let mut doc = Document {
            pages: vec![Page {
                blocks: vec![crate::model::Block {
                    kind: crate::model::BlockKind::Table,
                    content: "字段与类型".into(),
                    table: Some(crate::model::TableData {
                        rows: vec![
                            vec!["配置项说明".into(), "取值范围与默认行为说明".into()],
                            vec![
                                "超时重试次数决定请求失败前最多重发多少轮探测包。".into(),
                                "缓存容量上限决定热数据在内存里最多保留多少条记录。".into(),
                            ],
                        ],
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        detect_block_langs(&mut doc);
        assert_eq!(
            doc.pages[0].blocks[0].lang.as_deref(),
            Some("cmn"),
            "样本应取 table.rows（content 摘要不足 20 字符本会跳过）"
        );
    }
}
