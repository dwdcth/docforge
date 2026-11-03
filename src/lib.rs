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
pub mod engine;
pub mod error;
pub mod model;
#[cfg(feature = "ocr")]
pub mod ocr;
pub mod plugin;
pub mod plugins;
pub mod policy;
pub mod registry;
pub mod router;

pub use engine::DocumentEngine;
pub use error::{DocError, ErrorCode, Result};
pub use model::{
    Block, BlockKind, BlockSource, CoordUnits, DocMeta, Document, Image, Page, Rect, TableData,
};
pub use plugin::{
    BackendKind, FormatPlugin, ParseCapability, ProcessCapability, RawDoc, RawImage, RawPage,
    RawTextItem,
};
pub use registry::{Registry, RegistryError};

/// 便捷解析：默认 Registry 装配（文本族内置 + pdf feature 开启时注册
/// `PdfOptions::default()`，引擎自动取 `ocr-oar` feature 的全局 oar 实例）。
///
/// 自定义装配（注入引擎/选项/后端）用 [`Registry`] + [`DocumentEngine`]。
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let doc = docforge::parse("note.md")?;
/// println!("{} 页", doc.pages.len());
/// # Ok(()) }
/// ```
pub fn parse(path: &str) -> Result<Document> {
    let mut registry = Registry::new();
    plugins::text::register_defaults(&mut registry);
    #[cfg(feature = "docx")]
    crate::backends::office::register_defaults(&mut registry);
    #[cfg(feature = "epub")]
    crate::backends::epub::register_defaults(&mut registry);
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
    DocumentEngine::new(registry).parse(path)
}
