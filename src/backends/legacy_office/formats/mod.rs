//! 老式 Office 格式前端子模块（四细粒度特征各自门控）。

#[cfg(feature = "doc")]
pub mod doc;
#[cfg(feature = "ppt")]
pub mod ppt;
#[cfg(any(feature = "xls", feature = "xlsb"))]
pub mod sheet;
