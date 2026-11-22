//! 老式 Office 内部最小 XML 适配层：复用 `crate::math::xml`（零额外依赖），
//! 为 `xlsb` 的 OPC rels 与 VML 复选框读取提供 `Element` / `ns` / `parse_xml` 接口。

use super::super::error::ConvertError;
pub use crate::math::xml::Element;

pub mod ns {
    pub const PKG_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    pub const VML: &str = "urn:schemas-microsoft-com:vml";
    pub const X_VML: &str = "urn:schemas-microsoft-com:office:excel";
}

/// Strict OOXML 关系 URI 归一化为 Transitional 形式。
pub fn normalize_ooxml_uri(uri: &str) -> Option<String> {
    if let Some(rest) = uri.strip_prefix("http://purl.oclc.org/ooxml/") {
        return Some(format!("http://schemas.openxmlformats.org/{rest}"));
    }
    None
}

pub fn parse_xml(bytes: &[u8]) -> Result<Element, ConvertError> {
    let text = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        std::str::from_utf8(&bytes[3..])
            .map(|s| s.to_string())
            .map_err(|e| ConvertError::malformed(format!("invalid UTF-8 XML: {e}")))?
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        String::from_utf16_lossy(&units)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_be_bytes(c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned())
    };
    Ok(crate::math::xml::parse_xml(&text))
}

/// 为 `crate::math::xml::Element` 补充 `anydoc` 内部调用的扩展查询方法。
pub trait ElementExt {
    fn descendants<'a>(&'a self, namespace: &'a str, local: &'a str) -> Vec<&'a Element>;
    fn attr_unqualified(&self, local: &str) -> Option<&str>;
}

impl ElementExt for Element {
    fn descendants<'a>(&'a self, namespace: &'a str, local: &'a str) -> Vec<&'a Element> {
        let mut out = Vec::new();
        collect_descendants(self, namespace, local, &mut out);
        out
    }

    fn attr_unqualified(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(a_ns, a_loc, _)| a_ns.is_none() && a_loc == local)
            .map(|(_, _, v)| v.as_str())
    }
}

fn collect_descendants<'a>(
    e: &'a Element,
    namespace: &str,
    local: &str,
    out: &mut Vec<&'a Element>,
) {
    for child in e.child_elems() {
        if child.local == local && child.ns.as_deref() == Some(namespace) {
            out.push(child);
        }
        collect_descendants(child, namespace, local, out);
    }
}
