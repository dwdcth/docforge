//! 双语回写器（翻译闭环①的写侧）：按 `to_translation_units` 的 id 锚点把
//! 译文写回**原文档**，产出中英对照版。定位改写而非从零生成——原文件的
//! 样式 / 资源 / 非正文部件全部原样透传，只动正文 XML/XHTML。
//!
//! # 对齐契约（与提取侧同口径）
//!
//! * id 与 [`crate::emit::to_translation_units`] 一致：`p{页}-b{块序}`；
//! * 回写器**重解析原文件**（与提取侧同一解析器、同一默认选项）建立块序：
//!   同一解析器对同一文件产出稳定序（确定性解析），因此回写侧扫到的
//!   「第 k 个可翻译块」与提取侧同位；
//! * 可翻译判据 = [`crate::emit::is_translatable`] + `min_chars = 2` +
//!   空白文本跳过（与 `to_translation_units` 内联过滤完全同口径）；
//!   docx 侧表格块不占翻译位（表格内段落也不产块），epub 侧 Table/Image/Code
//!   不进翻译流但仍占块序号（锚点不重排）；
//! * 双保险：每个定位到的块会与重解析出的块文本做一致性比对，不一致即
//!   跳过该块（宁可漏写也不错位写）；
//! * `translated` 里对不上任何单元 id 的条目一律忽略（不报错）。
//!
//! # 三种策略（[`crate::emit::BiStrategy`]）
//!
//! * `BilingualInline`（默认）：译文插在原块之后（docx 复制原段 `<w:pPr>`；
//!   epub 一律插 `<p class="docforge-tr">`，heading 后也是 p——简化取舍）；
//! * `TranslationOnly`：替换原块文本。简化口径：docx 首 `w:t` 换译文、
//!   其余 `w:t` 清空（保留 run/书签结构，重解析文本等价于「其余 run 删」）；
//!   epub 首文本节点换译文、其余文本节点清空；
//! * `OriginalPlusAppendix`：正文不动，在末尾追加「译文」小节（docx 追加在
//!   body 末尾 sectPr 之前；epub 追加在最后一个 spine 文档 `</body>` 前）。
//!
//! # 接线（合并方执行，本模块不自行接线）
//!
//! `lib.rs` 加 `pub mod writeback;`；无新依赖、无新 feature——
//! 复用既有 `docx` / `epub` feature 门控（见下）。

#[cfg(feature = "docx")]
pub mod docx;
#[cfg(feature = "epub")]
pub mod epub;

#[cfg(feature = "docx")]
pub use docx::writeback_docx;
#[cfg(feature = "epub")]
pub use epub::writeback_epub;

/// XML / XHTML 文本节点的最小转义：`&` `<` `>`（属性场景不经过此函数——
/// 本模块只写固定属性值）。译文与追加段统一走这里。
pub(crate) fn escape_xml_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// id → 译文的查找表：重复 id 后者胜（HashMap 语义），未匹配 id 由调用方忽略。
/// 空白译文视为「未提供」，同样忽略（避免插入空段 / 清空原文）。
pub(crate) fn translation_map(translated: &[(String, String)]) -> std::collections::HashMap<&str, &str> {
    translated
        .iter()
        .map(|(id, tr)| (id.as_str(), tr.as_str()))
        .collect()
}

// ---------------------------------------------------------------------------
// ZIP 透传重打包（docx / epub 共用）
// ---------------------------------------------------------------------------

#[cfg(any(feature = "docx", feature = "epub"))]
mod zip_util {
    use std::io::{Read, Write};
    use std::path::Path;

    use crate::error::{DocError, Result};

    pub(crate) fn open_zip(path: &Path) -> Result<zip::ZipArchive<std::fs::File>> {
        let file = std::fs::File::open(path)
            .map_err(|e| DocError::corrupt(format!("cannot open {}: {e}", path.display())))?;
        zip::ZipArchive::new(file).map_err(|e| DocError::corrupt(format!("ZIP open failed: {e}")))
    }

    /// 逐条目透传原包字节，`replaced` 中的条目换成新字节，重打包到 `out`。
    /// EPUB 的 `mimetype` 约定（首条目 + Stored）在此统一维护。
    pub(crate) fn rewrite_zip(
        in_path: &Path,
        out_path: &Path,
        replaced: &[(String, Vec<u8>)],
    ) -> Result<()> {
        let mut zip = open_zip(in_path)?;

        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..zip.len() {
            let mut f = zip
                .by_index(i)
                .map_err(|e| DocError::corrupt(format!("ZIP 读取条目 {i} 失败: {e}")))?;
            if f.is_dir() {
                continue;
            }
            let name = f
                .name()
                .map_err(|e| DocError::corrupt(format!("ZIP 条目 {i} 名字损坏: {e}")))?
                .into_owned();
            let mut data = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut data)
                .map_err(|e| DocError::corrupt(format!("ZIP 读取 {name} 失败: {e}")))?;
            entries.push((name, data));
        }

        let apply = |name: &str, data: &mut Vec<u8>| {
            for (rn, rdata) in replaced {
                if rn.as_str() == name {
                    *data = rdata.clone();
                }
            }
        };

        let out_file = std::fs::File::create(out_path).map_err(|e| {
            DocError::corrupt(format!("cannot create {}: {e}", out_path.display()))
        })?;
        let mut zw = zip::ZipWriter::new(out_file);
        let mut wrote_mimetype = false;
        // mimetype 必须是首条目且 Stored（EPUB OCF 约定）
        if let Some((_, data)) = entries.iter().find(|(n, _)| n == "mimetype") {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("mimetype", opts)
                .map_err(|e| DocError::corrupt(format!("ZIP 写 mimetype 失败: {e}")))?;
            zw.write_all(data)
                .map_err(|e| DocError::corrupt(format!("ZIP 写 mimetype 失败: {e}")))?;
            wrote_mimetype = true;
        }
        for (name, mut data) in entries {
            if wrote_mimetype && name == "mimetype" {
                continue;
            }
            apply(&name, &mut data);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file(name.as_str(), opts)
                .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
            zw.write_all(&data)
                .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
        }
        zw.finish()
            .map_err(|e| DocError::corrupt(format!("ZIP 收尾失败: {e}")))?;
        Ok(())
    }
}

#[cfg(any(feature = "docx", feature = "epub"))]
pub(crate) use zip_util::{open_zip, rewrite_zip};

#[cfg(test)]
pub(crate) mod tests_util {
    //! 测试共用：最小转义的往返自检。
    use super::escape_xml_text;

    #[test]
    fn escape_xml_text_covers_amp_lt_gt() {
        assert_eq!(escape_xml_text("a & b <c> \"d\""), "a &amp; b &lt;c&gt; \"d\"");
        assert_eq!(escape_xml_text("中文《引号》"), "中文《引号》");
    }
}
