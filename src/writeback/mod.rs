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
//! docx / epub / html / fb2 / odt / xlsx / txt 七种（md 走
//! [`crate::emit::to_bilingual_markdown`] 纯文本重组，不经本模块）。txt 零依赖
//! 恒开；html 随 feature `html`、fb2 随 `fb2`、odt 随 `odt`（各自复用提取侧
//! feature 的解析管线，无新依赖）；xlsx 随 `xlsx`（格单元改写为 inlineStr，
//! 附录策略追加「译文」sheet，见 [`xlsx`] 模块头）。单文件编码取舍：txt 支持
//! UTF-8 逐字节
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
#[cfg(feature = "xlsx")]
pub mod xlsx;

/// PDF 双语附录载荷（合作形态：docforge 出 IR/译文序列，PDF 页生成在
/// 消费者侧——rumi 的 oxidize-pdf 双层经验）。
pub mod appendix;

/// 流式回写会话（push/finish：译文按单元喂入、spill 落盘、finish 打包——
/// 与 engine 的 parse_streaming 组成「解析→翻→写」流式闭环的最后一环）。
pub mod stream;

#[cfg(feature = "docx")]
pub use docx::{writeback_docx, writeback_docx_alternating};
#[cfg(feature = "epub")]
pub use epub::{writeback_epub, writeback_epub_alternating};
#[cfg(feature = "fb2")]
pub use fb2::writeback_fb2;
#[cfg(feature = "html")]
pub use html::writeback_html;
#[cfg(feature = "odt")]
pub use odt::writeback_odt;
pub use txt::writeback_txt;
#[cfg(feature = "xlsx")]
pub use xlsx::writeback_xlsx;

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

/// [`crate::emit::TranslationPair`] 的查找表：值 =（译文, ①侧源文本）。
/// 重复 id 后者胜、空白译文忽略——口径与既有 `(id, 译文)` 二元组时代一致；
/// 源校验（①→④ 文件变更边界）见 `TranslationPair` 的类型文档。
pub(crate) fn pair_map(
    pairs: &[crate::emit::TranslationPair],
) -> std::collections::HashMap<&str, (&str, Option<&str>)> {
    pairs
        .iter()
        .map(|p| (p.id.as_str(), (p.translation.as_str(), p.source.as_deref())))
        .collect()
}

/// 回写结果报告：「宁漏勿错」的可见化——写了多少、有译文但未写入的是谁、为什么。
///
/// `skipped` 只收**有译文但没写**的单元（无译文的单元是正常未翻译，不是
/// 跳过）；原因是人类可读中文短句（如「源文本不一致（提取后文件已变更）」
/// 「文本守卫不一致」「结构限制（嵌套表/重复格…）」）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WritebackReport {
    /// 实际写入的单元数（含表格格单元格）。
    pub written: usize,
    /// 有译文但未写入的单元：`(id, 原因)`。
    pub skipped: Vec<(String, String)>,
}

impl WritebackReport {
    /// 记一条跳过（链式便捷）。
    pub fn skip(&mut self, id: impl Into<String>, reason: &str) {
        self.skipped.push((id.into(), reason.to_string()));
    }
}

// ---------------------------------------------------------------------------
// 原子落盘（同目录 temp + rename）：崩溃 / 磁盘满（ENOSPC）不在目标路径留
// 半残文件。同目录临时兄弟文件保证 rename 的原子性（不跨分区——跨文件系统
// 的 rename 会退化为拷贝并可能中途失败）。
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};

/// 输出文件的临时兄弟文件名（同目录保证 rename 原子性不跨分区）：
/// `.{file_name}.tmp-{pid}-{nanos}`（隐藏件；pid + 纳秒双保险防同进程多会话
/// / 快速重建撞名，与 stream.rs 的 spill 命名同款策略）。
pub(crate) fn temp_sibling(out: &Path) -> PathBuf {
    let name = out
        .file_name()
        .map_or_else(|| "out".to_string(), |n| n.to_string_lossy().into_owned());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    out.with_file_name(format!(".{name}.tmp-{}-{nanos}", std::process::id()))
}

/// rename 临时文件到目标（原子落盘的收尾一步）；失败删除临时文件（不留
/// 垃圾）并上抛 `corrupt`。
pub(crate) fn commit_temp(tmp: &Path, out: &Path) -> crate::error::Result<()> {
    if let Err(e) = std::fs::rename(tmp, out) {
        let _ = std::fs::remove_file(tmp);
        return Err(crate::error::DocError::corrupt(format!(
            "cannot create {}: {e}",
            out.display()
        )));
    }
    Ok(())
}

/// 字节写出的原子化（txt / html / fb2 / md 单文件写出共用）：先写同目录
/// 临时兄弟文件，成功后 [`commit_temp`] rename 到位。写出中途失败（崩溃 /
/// ENOSPC）删除临时件——目标路径要么是完整的旧/新内容，要么不存在，
/// 永远不留半残文件。
pub(crate) fn write_bytes_atomic(out: &Path, bytes: &[u8]) -> crate::error::Result<()> {
    let tmp = temp_sibling(out);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(crate::error::DocError::corrupt(format!(
            "cannot create {}: {e}",
            out.display()
        )));
    }
    commit_temp(&tmp, out)
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
    ///
    /// 未被替换命中的条目走 [`zip::ZipWriter::raw_copy_file`]——原压缩流
    /// 直拷（不解压不重压，CRC / 时间戳 / 权限 / 扩展字段原样），100MB 图多
    /// 的容器回写从秒级降到毫秒级；命中条目照旧写新字节。EPUB 的 `mimetype`
    /// 约定（首条目 + Stored）在此统一维护：mimetype 只有 20 字节级、不走
    /// raw copy（raw copy 保留原压缩态，缺损包会带着非 Stored 的 mimetype
    /// 进输出，违反 OCF 约定），照旧解出字节强制 Stored。
    ///
    /// 输出经 [`super::temp_sibling`] 的同目录临时文件落盘，`finish` 成功后
    /// [`super::commit_temp`] rename 到位——中途失败 / ENOSPC 不在目标路径
    /// 留半残 zip。重复的替换名按「后者胜」收敛（与旧 `apply` 语义一致）。
    pub(crate) fn rewrite_zip(
        in_path: &Path,
        out_path: &Path,
        replaced: &[(String, Vec<u8>)],
    ) -> Result<()> {
        let mut zip = open_zip(in_path)?;

        // 预读条目名（目录条目照旧丢弃——输出不携带目录条目）；mimetype
        // 的字节顺手读出（20 字节级，强制 Stored 重写），其余条目不预读
        // 数据——未命中替换的条目稍后 raw copy 压缩流直拷。
        let mut entries: Vec<(usize, String)> = Vec::new();
        let mut mimetype_data: Option<Vec<u8>> = None;
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
            if name == "mimetype" {
                let mut data = Vec::with_capacity(f.size() as usize);
                f.read_to_end(&mut data)
                    .map_err(|e| DocError::corrupt(format!("ZIP 读取 {name} 失败: {e}")))?;
                mimetype_data = Some(data);
            }
            entries.push((i, name));
        }

        let tmp = super::temp_sibling(out_path);
        let out_file = std::fs::File::create(&tmp).map_err(|e| {
            DocError::corrupt(format!("cannot create {}: {e}", tmp.display()))
        })?;
        let mut zw = zip::ZipWriter::new(out_file);
        // 写入段收进闭包：任一条目写失败直接返回 Err，外层统一删临时件
        //（闭包内 by_index 可变借用 zip，需 FnMut 可变绑定）
        let mut write = |zw: &mut zip::ZipWriter<std::fs::File>| -> Result<()> {
            let mut wrote_mimetype = false;
            // mimetype 必须是首条目且 Stored（EPUB OCF 约定）
            if let Some(data) = &mimetype_data {
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zw.start_file("mimetype", opts)
                    .map_err(|e| DocError::corrupt(format!("ZIP 写 mimetype 失败: {e}")))?;
                zw.write_all(data)
                    .map_err(|e| DocError::corrupt(format!("ZIP 写 mimetype 失败: {e}")))?;
                wrote_mimetype = true;
            }
            for (idx, name) in &entries {
                if wrote_mimetype && name == "mimetype" {
                    continue;
                }
                if let Some((_, data)) =
                    replaced.iter().rev().find(|(n, _)| n == name)
                {
                    // 命中替换：新字节照旧走 start_file（重压缩）
                    let opts = zip::write::SimpleFileOptions::default();
                    zw.start_file(name.as_str(), opts)
                        .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
                    zw.write_all(data)
                        .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
                } else {
                    // 未触碰条目：压缩流直拷（不解压不重压）
                    let f = zip
                        .by_index(*idx)
                        .map_err(|e| DocError::corrupt(format!("ZIP 读取条目 {idx} 失败: {e}")))?;
                    zw.raw_copy_file(f)
                        .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
                }
            }
            Ok(())
        };
        match write(&mut zw) {
            Ok(()) => match zw.finish() {
                Ok(_) => super::commit_temp(&tmp, out_path),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    Err(DocError::corrupt(format!("ZIP 收尾失败: {e}")))
                }
            },
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
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

// ---------------------------------------------------------------------------
// 测试：原子落盘 helper + raw copy 直拷
// ---------------------------------------------------------------------------

#[cfg(test)]
mod atomic_write_tests {
    use super::{commit_temp, temp_sibling, writeback_txt};
    use std::path::{Path, PathBuf};

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_atomic_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 目录里含 `.tmp-` 的残留文件名（端到端断言用）。
    fn tmp_residue(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect()
    }

    /// 临时兄弟文件：同目录、隐藏件、带 pid 标记、两次调用不撞名。
    #[test]
    fn temp_sibling_is_hidden_sibling_in_same_dir() {
        let p = temp_sibling(Path::new("/tmp/somewhere/out.epub"));
        assert_eq!(p.parent(), Some(Path::new("/tmp/somewhere")), "同目录（rename 原子性）");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".out.epub.tmp-"), "隐藏件：{name}");
        assert!(name.contains(&format!("tmp-{}-", std::process::id())), "带 pid：{name}");
        // 隔开纳秒时间戳（防粗粒度时钟下两次调用同名）
        std::thread::sleep(std::time::Duration::from_millis(1));
        let p2 = temp_sibling(Path::new("/tmp/somewhere/out.epub"));
        assert_ne!(p, p2, "两次调用不撞名");
    }

    /// commit_temp 成功路径：tmp 内容落位 out、tmp 消失。
    #[test]
    fn commit_temp_renames_and_removes_tmp() {
        let dir = case_dir("ok");
        let tmp = dir.join(".out.txt.tmp-x");
        std::fs::write(&tmp, b"hello").unwrap();
        let out = dir.join("out.txt");
        commit_temp(&tmp, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"hello");
        assert!(!tmp.exists(), "临时文件已消失");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// commit_temp 失败路径（目标目录不存在）：Err 且 tmp 被清理（不留垃圾）。
    #[test]
    fn commit_temp_failure_removes_tmp() {
        let dir = case_dir("fail");
        let tmp = dir.join(".out.txt.tmp-x");
        std::fs::write(&tmp, b"hello").unwrap();
        let out = dir.join("no-such-dir/out.txt");
        let err = commit_temp(&tmp, &out).unwrap_err();
        assert_eq!(err.code.as_str(), "corrupt");
        assert!(!tmp.exists(), "失败路径清理临时文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端：回写成功后 out 目录无 `.tmp-` 残留文件（txt 全流程）。
    #[test]
    fn writeback_leaves_no_tmp_residue() {
        let dir = case_dir("e2e");
        let in_path = dir.join("in.txt");
        std::fs::write(&in_path, "第一章 起\n\n正文一段句子。\n").unwrap();
        let out_path = dir.join("out.txt");
        writeback_txt(
            &in_path,
            &[("p0-b0".to_string(), "The beginning.".to_string()).into()],
            crate::emit::BiStrategy::BilingualInline,
            &out_path,
        )
        .unwrap();
        assert!(out_path.exists());
        assert!(std::fs::read_to_string(&out_path).unwrap().contains("The beginning."));
        assert!(tmp_residue(&dir).is_empty(), "无临时件残留");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// raw copy 直拷（共用 [`super::zip_util::rewrite_zip`]，docx / epub / odt
/// 自动受益）：未触碰条目压缩态与内容原样直拷。
#[cfg(all(test, any(feature = "docx", feature = "epub", feature = "odt")))]
mod zip_raw_copy_tests {
    use super::{open_zip, rewrite_zip};
    use std::io::{Read as _, Write as _};
    use std::path::{Path, PathBuf};

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_ziputil_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry_bytes(path: &Path, name: &str) -> Vec<u8> {
        let mut z = open_zip(path).unwrap();
        let mut buf = Vec::new();
        z.by_name(name).unwrap().read_to_end(&mut buf).unwrap();
        buf
    }

    /// 未触碰条目（伪图片字节，Stored）内容与压缩态原样直拷、触碰条目换
    /// 新字节、mimetype 仍居首且强制 Stored；条目序不变；输出目录无临时件
    /// 残留。旧实现把全部条目重新 deflate——Stored 条目会变 Deflated，本测
    /// 的压缩态断言即 raw copy 的新现实。
    #[test]
    fn raw_copy_keeps_untouched_entries_verbatim() {
        let dir = case_dir("rawcopy");
        let in_path = dir.join("in.zip");
        let image: Vec<u8> = (0..65536usize).map(|i| (i % 251) as u8).collect();
        {
            let f = std::fs::File::create(&in_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("mimetype", stored).unwrap();
            zw.write_all(b"application/epub+zip").unwrap();
            zw.start_file("OEBPS/pic.bin", stored).unwrap();
            zw.write_all(&image).unwrap();
            zw.start_file("OEBPS/text.xml", zip::write::SimpleFileOptions::default()).unwrap();
            zw.write_all(b"<p>body text</p>").unwrap();
            zw.finish().unwrap();
        }

        let out_path = dir.join("out.zip");
        rewrite_zip(
            &in_path,
            &out_path,
            &[("OEBPS/text.xml".to_string(), b"<p>replaced</p>".to_vec())],
        )
        .unwrap();

        let zin = open_zip(&in_path).unwrap();
        let mut zout = open_zip(&out_path).unwrap();
        assert_eq!(zout.len(), zin.len(), "条目数不变");
        assert_eq!(zout.by_index(0).unwrap().name().unwrap(), "mimetype", "mimetype 仍居首");
        assert_eq!(
            zout.by_index(0).unwrap().compression(),
            zip::CompressionMethod::Stored,
            "mimetype 强制 Stored（不走 raw copy，保 OCF 约定）"
        );

        // 未触碰条目：内容字节一致 + 压缩态原样（raw copy 直拷）
        assert_eq!(entry_bytes(&in_path, "OEBPS/pic.bin"), entry_bytes(&out_path, "OEBPS/pic.bin"));
        assert_eq!(
            zout.by_name("OEBPS/pic.bin").unwrap().compression(),
            zip::CompressionMethod::Stored,
            "未触碰条目压缩态直拷（旧实现会重新 deflate 成 Deflated）"
        );
        // 触碰条目已改
        assert_eq!(entry_bytes(&out_path, "OEBPS/text.xml"), b"<p>replaced</p>");

        // 原子落盘收尾：目录无临时件残留
        let residue: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(residue.is_empty(), "{residue:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
