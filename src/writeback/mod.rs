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
//! * 表格（wave2）：单元流含格单元（`kind = Table`，锚点 `…-r{行}c{列}`），
//!   回写侧按文档序「格位 ↔ 格单元」配对（格文本一致性守卫同段落款），
//!   跨列/合并 span 解析侧不展开（一格一列），回写侧同口径镜像——锚点
//!   权威在 `TableData.rows` 下标，两侧必须逐字节同网格。txt/fb2 无表格
//!   结构恒久跳过格单元；epub 单节坍缩形态不产 Table 块（见 epub 模块头）；
//! * 双保险：每个定位到的块会与重解析出的块文本做一致性比对，不一致即
//!   跳过该块（宁可漏写也不错位写）；
//! * `translated` 里对不上任何单元 id 的条目一律忽略（不报错）。
//!
//! # 三种策略（[`crate::emit::BiStrategy`]）
//!
//! * `BilingualInline`（默认）：译文插在原块之后（docx 复制原段 `<w:pPr>`；
//!   epub/html 一律插 `<p class="docforge-tr">`；odt 插 `<text:p>` 并沿用原段
//!   `text:style-name`；fb2 在 p/subtitle/title 元素后插同级 `<p>`（poem 内
//!   不合法则跳过）；txt 原段行后紧跟纯文本译文行）；
//! * `TranslationOnly`：替换原块文本。简化口径：docx 首 `w:t` 换译文、
//!   其余 `w:t` 清空（保留 run/书签结构，重解析文本等价于「其余 run 删」）；
//!   epub/html 首文本节点换译文、其余文本节点清空；fb2 单元素换内文本
//!   （title/stanza 首 span 换译、其余清空）；odt/fb2 元素内文本区整体替换；
//!   txt 块行区间换译文单行；
//! * `OriginalPlusAppendix`：正文不动，在末尾追加「译文」小节（docx 追加在
//!   body 末尾 sectPr 之前；epub 追加在最后一个 spine 文档 `</body>` 前；
//!   html 追加在 `</body>` 前；fb2 追加主文 `</body>` 前的 `<section>`；
//!   odt 追加在 `</office:text>` 前；txt 文尾追加 `【译文】` 分隔 + 译文段）。
//!
//! # 覆盖格式与门控
//!
//! docx / epub / html / fb2 / odt / txt 六种（md 走
//! [`crate::emit::to_bilingual_markdown`] 纯文本重组，不经本模块）。txt 零依赖
//! 恒开；html 随 feature `html`、fb2 随 `fb2`、odt 随 `odt`（各自复用提取侧
//! feature 的解析管线，无新依赖）。单文件编码取舍：txt 支持 UTF-8 逐字节
//! 保真 + 解码链命中件转存 UTF-8（lossy 兜底件拒绝）；html / fb2 仅 UTF-8
//! （避免文件内编码声明失配）；zip 容器格式（docx/epub/odt）由共用
//! `rewrite_zip` 字节透传非正文部件。

#[cfg(feature = "docx")]
pub mod docx;
#[cfg(feature = "epub")]
pub mod epub;
#[cfg(feature = "fb2")]
pub mod fb2;
#[cfg(feature = "html")]
pub mod html;
#[cfg(feature = "odt")]
pub mod odt;
pub mod txt;

/// PDF 双语附录载荷（合作形态：docforge 出 IR/译文序列，PDF 页生成在
/// 消费者侧——rumi 的 oxidize-pdf 双层经验）。
pub mod appendix;

/// 流式回写会话（push/finish：译文按单元喂入、spill 落盘、finish 打包——
/// 与 engine 的 parse_streaming 组成「解析→翻→写」流式闭环的最后一环）。
pub mod stream;

#[cfg(feature = "docx")]
pub use docx::writeback_docx;
#[cfg(feature = "epub")]
pub use epub::writeback_epub;
#[cfg(feature = "fb2")]
pub use fb2::writeback_fb2;
#[cfg(feature = "html")]
pub use html::writeback_html;
#[cfg(feature = "odt")]
pub use odt::writeback_odt;
pub use txt::writeback_txt;

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

/// 一处回写编辑（xml / xhtml 单文件改写共用；txt 走行级重组不经此）。
pub(crate) enum Edit {
    /// 在 `pos` 前插入。
    Insert(usize, String),
    /// 把 `[start, end)` 替换为文本。
    Replace(usize, usize, String),
}

/// 按 (起, 止, 文本) / (位置, 文本) 从后往前应用编辑（先排序；区间互不重叠）。
pub(crate) fn apply_edits(xml: &str, edits: Vec<Edit>) -> String {
    let mut items: Vec<(usize, usize, String)> = edits
        .into_iter()
        .map(|e| match e {
            Edit::Insert(p, s) => (p, p, s),
            Edit::Replace(s, e, t) => (s, e, t),
        })
        .collect();
    items.sort_by_key(|(s, _, _)| *s);
    let mut out = String::with_capacity(xml.len());
    let mut cursor = 0usize;
    for (s, e, txt) in items {
        if s < cursor {
            continue; // 区间重叠（不应发生）：保守丢弃后到的
        }
        out.push_str(&xml[cursor..s]);
        out.push_str(&txt);
        cursor = e;
    }
    out.push_str(&xml[cursor..]);
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

#[cfg(any(feature = "docx", feature = "epub", feature = "odt"))]
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

#[cfg(any(feature = "docx", feature = "epub", feature = "odt"))]
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
