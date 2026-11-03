//! `docforge`：通用文档解析 crate——统一块级 IR + 格式路由 + PDF 页级混合路由。
//!
//! 设计吸收自 docparser-core 0.2.1（IR / 两层能力插件 / Registry / Router /
//! BackendPolicy / Engine，见 THIRD_PARTY_NOTICES.md），在此基础上：
//!
//! - IR 扩展 OCR 语义：`Block.confidence`（行置信度）、`Block.source`
//!   （text_layer / ocr / native）、`Page.coord_units`（坐标单位显式化）；
//! - PDF 后端走页级混合路由（文字层直提 vs 渲染+OCR，设计参考 rumi-ocr
//!   pdf_route，M1 落地）；
//! - 输出面向三类消费者：解析/格式转换（JSON / Markdown emit）、OCR
//!   （置信度与来源进 IR）、翻译（lang-detect、段落级翻译友好 emit，M2+）。
//!
//! 本 crate 零第三方运行依赖（仅 serde 用于 JSON 往返）。
//!
//! # 示例
//!
//! ```no_run
//! use docforge::{plugins, DocumentEngine, Registry};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut registry = Registry::new();
//! plugins::text::register_defaults(&mut registry); // txt / md / csv / html
//! let engine = DocumentEngine::new(registry);
//!
//! let doc = engine.parse("note.md")?;
//! println!("{} 页 / {} 字符", doc.pages.len(), doc.meta.text_length);
//! for block in &doc.pages[0].blocks {
//!     println!("{:?}: {}", block.kind, block.content);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! 接入其它格式只需注册对应后端（`backends::pdf::register` 等，M1+ 提供），
//! 见 [`Registry`] 与 [`DocumentEngine`]。

pub mod backends;
pub mod emit;
pub mod encoding;
pub mod engine;
pub mod error;
pub mod model;
#[cfg(feature = "ocr")]
pub mod ocr;
pub mod plugin;
pub mod plugins;
pub mod writeback;
pub mod policy;
pub mod registry;
pub mod router;

pub use engine::{DocumentEngine, PageSink, ParsePhase, ProgressFn};
pub use error::{DocError, ErrorCode, Result};
// 编码探测（横切能力）：decode 对外兼容（原 plugins::text::decode），
// detect_and_decode 返回"解码结果 + 命中编码名"（rumi 等外部消费者入口）
pub use encoding::{decode, detect_and_decode};
pub use model::{
    Block, BlockKind, BlockSource, CoordUnits, DocMeta, Document, Image, Page, Rect, TableData,
};
pub use plugin::{
    BackendKind, FormatPlugin, ParseCapability, ProcessCapability, RawDoc, RawImage, RawPage,
    RawTextItem,
};
pub use registry::{Registry, RegistryError};

/// 默认 Registry 装配（文本族内置 + pdf feature 开启时注册
/// `PdfOptions::default()`）。发布版不内置 OCR 引擎（git 依赖不上 crates.io），
/// 引擎一律注入：见 [`ocr::OcrEngine`]。[`parse`] / [`parse_with_progress`] /
/// [`parse_streaming`] 共用此装配；自定义装配（注入引擎/选项/后端）用
/// [`Registry`] + [`DocumentEngine`]。
fn default_registry() -> Registry {
    let mut registry = Registry::new();
    plugins::text::register_defaults(&mut registry);
    #[cfg(feature = "docx")]
    crate::backends::office::register_defaults(&mut registry);
    #[cfg(feature = "epub")]
    crate::backends::epub::register_defaults(&mut registry);
    #[cfg(feature = "archive")]
    crate::backends::archive::register_defaults(&mut registry);
    #[cfg(feature = "chm")]
    crate::backends::chm::register_defaults(&mut registry);
    #[cfg(feature = "cbz")]
    crate::backends::cbz::register_defaults(&mut registry);
    #[cfg(feature = "image-files")]
    crate::backends::image_files::register_defaults(&mut registry);
    #[cfg(feature = "fb2")]
    crate::backends::fb2::register_defaults(&mut registry);
    #[cfg(feature = "mobi")]
    crate::backends::mobi::register_defaults(&mut registry);
    #[cfg(feature = "djvu")]
    {
        let opts = crate::backends::djvu::DjVuOptions {
            #[cfg(feature = "ocr-oar")]
            engine: Some(crate::ocr::oar::OarOcrEngine::shared()),
            #[cfg(not(feature = "ocr-oar"))]
            engine: None,
        };
        crate::backends::djvu::register(&mut registry, opts);
    }
    #[cfg(feature = "pdf")]
    {
        let opts = crate::backends::pdf::PdfOptions {
            #[cfg(feature = "ocr-oar")]
            engine: Some(crate::ocr::oar::OarOcrEngine::shared()),
            #[cfg(not(feature = "ocr-oar"))]
            engine: None,
            ..Default::default()
        };
        crate::backends::pdf::register(&mut registry, opts);
    }
    registry
}

/// 便捷解析：默认 Registry 装配。
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let doc = docforge::parse("note.md")?;
/// println!("{} 页", doc.pages.len());
/// # Ok(()) }
/// ```
pub fn parse(path: &str) -> Result<Document> {
    DocumentEngine::new(default_registry()).parse(path)
}

/// 便捷解析 + 引擎级进度钩子：[`parse`] 的默认装配外加
/// [`DocumentEngine::with_progress`]——大文件黑盒格式（大 CHM / epub /
/// 压缩包等无页级回调的后端）的最小进度观测面。只通知不取消；页级
/// 细粒度与取消由对应后端 Options 提供（如 `PdfOptions::on_progress`），
/// 与本钩子并存。
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use docforge::{parse_with_progress, ParsePhase};
/// use std::sync::Arc;
/// let doc = parse_with_progress("big.chm", Arc::new(|phase: ParsePhase, detail: &str| {
///     println!("{phase:?}: {detail}");
/// }))?;
/// println!("{} 页", doc.pages.len());
/// # Ok(()) }
/// ```
pub fn parse_with_progress(path: &str, cb: ProgressFn) -> Result<Document> {
    DocumentEngine::new(default_registry())
        .with_progress(cb)
        .parse(path)
}

/// 便捷流式解析：默认 Registry 装配（与 [`parse`] / [`parse_with_progress`]
/// 共用）。assemble 后逐页投递给 sink、按已投递页回填统计，语义详见
/// [`DocumentEngine::parse_streaming`]。
///
/// 消费指引：pdf 千页扫描件（keep_raster_pages 全量驻留爆内存）的最坏场景
/// 直接用 pdf 后端的深度流式入口 `docforge::backends::pdf::parse_streaming`
///（页产即吐、峰值 = 单页）；其余格式（及对 pdf 的正确但不省内存路径）用
/// 本函数。
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let meta = docforge::parse_streaming("note.md", &mut |page| {
///     println!("{} 字符", page.text.chars().count());
///     true // false = 中止：余页不产，meta 仍返回
/// })?;
/// println!("{} 页", meta.page_count);
/// # Ok(()) }
/// ```
pub fn parse_streaming(path: &str, sink: PageSink<'_>) -> Result<DocMeta> {
    DocumentEngine::new(default_registry()).parse_streaming(path, sink)
}
