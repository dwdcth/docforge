// 吸收自 firecrawl/anydoc 0.2.5（MIT，`src/formats/sheet/mod.rs`），见 THIRD_PARTY_NOTICES.md。
//! XLS（BIFF OLE2）与 XLSB（二进制 SpreadsheetML）后端入口。

mod common;
mod controls;
mod numfmt;
#[cfg(feature = "xls")]
pub mod xls;
#[cfg(feature = "xlsb")]
pub mod xlsb;

#[cfg(feature = "xls")]
use crate::backends::legacy_office::error::ConvertError;
#[cfg(feature = "xls")]
use crate::backends::legacy_office::model::Document;

#[cfg(feature = "xlsb")]
pub fn parse_xlsb(bytes: &[u8]) -> Result<crate::backends::legacy_office::model::Document, crate::backends::legacy_office::error::ConvertError> {
    use crate::backends::legacy_office::error::ConvertError;
    use crate::backends::legacy_office::package::relationships::{read_rels, rel_type};
    use crate::backends::legacy_office::package::{path, Package};

    let mut pkg = Package::open(bytes)?;
    let wb_part = read_rels(&mut pkg, "_rels/.rels")?
        .first_of_type(rel_type::OFFICE_DOCUMENT)
        .and_then(|rel| path::resolve("", &rel.target).ok())
        .map(|t| t.path)
        .or_else(|| pkg.has_part("xl/workbook.bin").then(|| "xl/workbook.bin".to_string()))
        .ok_or_else(|| ConvertError::malformed("not a readable xlsb workbook"))?;
    xlsb::parse(&mut pkg, &wb_part)
}

#[cfg(feature = "xls")]
pub fn parse_xls(bytes: &[u8]) -> Result<Document, ConvertError> {
    // 加密 OOXML（EncryptedPackage）也是 OLE2 容器，先判定加密
    if let Ok(ole) = cfb::CompoundFile::open(std::io::Cursor::new(bytes)) {
        if ole
            .read_root_storage()
            .any(|e| e.name().eq_ignore_ascii_case("EncryptedPackage"))
        {
            return Err(ConvertError::Encrypted);
        }
    }
    xls::parse(bytes)
}

pub(super) fn format_float(f: f64) -> String {
    match format!("{f:.14e}").parse::<f64>() {
        Ok(rounded) => format!("{rounded}"),
        Err(_) => format!("{f}"),
    }
}

pub(super) fn format_time_of_day(days: f64) -> String {
    let total_secs = (days.abs() * 86_400.0).round() as u64 % 86_400;
    let (h, m, s) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

pub(super) fn format_duration_days(days: f64) -> String {
    let sign = if days < 0.0 { "-" } else { "" };
    let total_secs = (days.abs() * 86_400.0).round() as u64;
    let (h, m, s) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
    format!("{sign}{h}:{m:02}:{s:02}")
}

pub(super) fn rk_number(rk: u32) -> f64 {
    let value = if rk & 0x02 != 0 {
        f64::from((rk as i32) >> 2)
    } else {
        f64::from_bits(u64::from(rk & 0xFFFF_FFFC) << 32)
    };
    if rk & 0x01 != 0 {
        value / 100.0
    } else {
        value
    }
}

pub(super) fn error_literal(code: u8) -> Option<&'static str> {
    Some(match code {
        0x00 => "#NULL!",
        0x07 => "#DIV/0!",
        0x0F => "#VALUE!",
        0x17 => "#REF!",
        0x1D => "#NAME?",
        0x24 => "#NUM!",
        0x2A => "#N/A",
        0x2B => "#GETTING_DATA",
        _ => return None,
    })
}
