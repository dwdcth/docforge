//! 格式后端（feature 门控注册，docparse-plan §7）。
//!
//! 裸内核（default-features = false）不含任何后端，仅 IR/引擎/文本族底座。

#[cfg(feature = "docx")]
pub mod office;
#[cfg(feature = "epub")]
pub mod epub;
#[cfg(feature = "pdf")]
pub mod pdf;
