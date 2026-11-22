//! 容器与限额适配层（老式 Office 内部使用）。

pub use crate::limits;

#[cfg(feature = "xlsb")]
pub mod archive;
#[cfg(feature = "xlsb")]
pub mod path;
#[cfg(feature = "xlsb")]
pub mod relationships;
#[cfg(feature = "xlsb")]
pub mod xml;

#[cfg(feature = "xlsb")]
pub use archive::Package;
