//! 格式后端（feature 门控注册，docparse-plan §7）。
//!
//! 裸内核（default-features = false）不含任何后端，仅 IR/引擎/文本族底座。

#[cfg(any(feature = "docx", feature = "pptx", feature = "xlsx"))]
pub mod office;
#[cfg(feature = "epub")]
pub mod epub;

#[cfg(any(feature = "odt", feature = "ods", feature = "odp"))]
pub mod odf;
#[cfg(feature = "cbz")]
pub mod cbz;
#[cfg(feature = "archive")]
pub mod archive;
#[cfg(feature = "chm")]
pub mod chm;
#[cfg(feature = "image-files")]
pub mod image_files;
#[cfg(feature = "djvu")]
pub mod djvu;
#[cfg(feature = "fb2")]
pub mod fb2;
#[cfg(feature = "mobi")]
pub mod mobi;
#[cfg(feature = "pdf")]
pub mod pdf;
