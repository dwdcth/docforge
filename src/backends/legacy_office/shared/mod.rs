//! 老式 Office 共享基础设施（按特性门控裁剪）。

pub mod assets;
pub mod binary;
#[cfg(any(feature = "doc", feature = "ppt"))]
pub mod blockstyle;
#[cfg(any(feature = "doc", feature = "ppt"))]
pub mod delta;
#[cfg(feature = "doc")]
pub mod fields;
#[cfg(feature = "doc")]
pub mod grid;
pub mod header;
#[cfg(any(feature = "doc", feature = "ppt"))]
pub mod list;
#[cfg(feature = "doc")]
pub mod numbering;
#[cfg(any(feature = "doc", feature = "ppt", feature = "xls"))]
pub mod officeart;
pub mod text;
#[cfg(any(feature = "doc", feature = "ppt"))]
pub mod uri;
