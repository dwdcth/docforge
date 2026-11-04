// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! 内置插件（纯文本族）：txt / md / csv / html / 字幕 / rtf。
//!
//! PDF 与 OOXML 后端在 `docparser-pdf` / `docparser-office` 两个独立 crate，
//! 通过各自的 `register_defaults` 注册，核心层不含任何格式实现之外的 native 依赖。
//!
//! `md` / `csv` / `html` / `rtf` 由同名 cargo feature 控制（默认全开）；`txt` 是底座，始终编译。
//! 字幕（srt/vtt/ass/ssa）无第三方依赖，随 `txt` 始终编译。
//! 只要「字节流 → IR」的最小内核时用 `default-features = false`。

pub mod chapter;
#[cfg(feature = "csv")]
pub mod csv;
#[cfg(feature = "html")]
pub mod html;
#[cfg(feature = "lang-detect")]
pub mod lang_detect;
#[cfg(feature = "md")]
pub mod markdown;
#[cfg(feature = "rtf")]
pub mod rtf;
pub mod running_detect;
pub mod subtitle;
pub mod text;

pub use text::register_defaults;
