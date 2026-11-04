// docforge 新增（翻译闭环①写侧）：EPUB 双语回写。
//! EPUB 双语回写：ZIP 透传 + spine 各 XHTML 重块化对齐 + 定位改写。
//!
//! # 对齐口径（与 `backends/epub.rs` 提取侧逐条对应）
//!
//! * 提取侧管线：`EpubRawBackend`（spine 阅读序：linear="no" / .ncx / 缺件跳过）
//!   → `EpubPlugin::assemble`（每 spine 文档 `parse_blocks` 块化 → 按标题切节；
//!   单节文档坍缩成一段）→ `to_translation_units`。id 的页名来自该 Document
//!   （toc 标题或文档内标题），所以回写侧**重跑同一管线**拿权威 id/文本，
//!   再以同一套块化/切节口径把每个 Document 页映回「spine 文档 × raw 块序」：
//!   - 切节页（多节文档）：页内第 b 块 ↔ 该节的 raw 块下标（标题块被切节
//!     排除，与提取侧一致——这类标题本就不产翻译单元）；
//!   - 坍缩页（单节文档）：单元文本 = 全部非空块文本按 `\n\n` 连接；回写
//!     定位取**全部非空 raw 块**（BilingualInline 插在最后一块之后；
//!     TranslationOnly 首块首文本节点换译文、其余可翻译块的文本节点清空）。
//! * XHTML 定位用与 `plugins/html.rs::parse_blocks` 同口径的扫描（同款触发
//!   顺序 / 空白跳过 / 实体解码），带字节 span。简化取舍：BilingualInline
//!   一律插 `<p class="docforge-tr">`（heading 后也是 p；`</li>` 后插入落在
//!   列表容器内，阅读器容忍）；TranslationOnly 只动可翻译块的文本节点，
//!   Image/Code 结构块原样保留。
//! * 表格格级回写（wave2，与 `writeback::html` 同构，机制详见其模块头
//!   「表格格级回写」）：切节页的 Table 块在表格位逐格消费格单元——扫描的
//!   非空格序列与提取侧格单元同序同数（解析侧无 colspan 展开：td/th 文档序
//!   即列序），逐格配对 + 格文本守卫（不一致跳过该格不写，宁漏勿错）；
//!   TranslationOnly 格内首文本节点换译、其余清空，BilingualInline 格内
//!   `<br class="docforge-tr"/>` 分隔原/译，OriginalPlusAppendix 表格原样
//!   （格译文不进附录）。
//! * **单节坍缩边界**：`EpubPlugin::assemble`（Document 口径）把单节 spine
//!   文档坍缩成一段（`_xhtml_text` 回退同款，表格文本并入段落单元），该形态
//!   不产 Table 块 → 格单元不存在，格替换自动无操作（表格文本仍残留在坍缩
//!   段文本里）；多节（切节）文档表格保留结构块，格替换生效。
//! * 双保险：单元文本与重块化文本一致性比对，不一致跳过该单元（宁漏勿错）；
//!   非 UTF-8 的 spine 条目（罕见，如 GBK 声明件）不做改写（避免改写后
//!   字节与编码声明失配），其余条目照常透传。
//! * 源校验（①→④ 边界）：`TranslationPair` 的 `source` 携带提取侧源文本
//!   时，写入前与当前文本比对，不一致跳过并记入 `WritebackReport`——①与
//!   ④两次解析之间文件被改时位置锚点会指向别的文本（静默错位写是最恶劣
//!   故障模式）；`source: None` = 信任锚点（原行为）。报告口径见 docx 模块
//!   头「源校验」。
//! * 未匹配 id / 空白译文一律忽略。
//!
//! # 部件级交替（[`writeback_epub_alternating`]，rumi「一页原文一页译文」）
//!
//! EPUB 的 spine 部件天然是章/页边界——每个**有命中译文**的 spine 部件后
//! 插入「该部件的纯译文部件」，阅读器里即 真·逐部件交替（三策略里的
//! OriginalPlusAppendix 对这个体验只是退化形态）。与 [`writeback_epub`]
//! 共用同一套解析/重块化/provenance/编辑管线（[`plan_epub_edits`]，策略
//! 固定 TranslationOnly），差异只在收尾：
//!
//! * 译文部件内容 = 该部件全部命中单元的 TranslationOnly 渲染；部件是完整
//!   合法 XHTML（head/样式/结构块原样），未命中单元原样保留——不是只译文
//!   的碎片页；
//! * 命名 `{原部件名去扩展}.tr.xhtml`，与原部件**同目录**——原部件 head
//!   里 `<link>` 的相对 href 天然不变（无相对路径重算这一坑）；
//! * OPF：manifest 追加 `<item id="{原id}-tr" …/>`（撞名则 -tr2、-tr3…），
//!   spine 在原 `<itemref>` 后插 `<itemref idref="{原id}-tr"/>`；原部件
//!   字节不动（只有 OPF 替换与新增译文部件两处编辑）；
//! * 无任何命中单元（无实际编辑）的部件不插译文部件——避免逐页「相同页
//!   翻两遍」；OPF 非 UTF-8 整体拒绝（译文部件的挂载骨架不可缺，宁响勿哑）；
//! * v1 不动 nav.xhtml / toc.ncx：目录保持原文结构不膨胀，线性翻页为主。

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::backends::epub::{EpubPlugin, EpubRawBackend};
use crate::emit::BiStrategy;
use crate::error::{DocError, Result};
use crate::model::{Block, BlockKind};
use crate::plugin::{FormatPlugin, ParseCapability, RawDoc};

use super::html::{scan_html_blocks, HtmlSlot, TR_CLASS};
use super::{apply_edits, Edit, escape_xml_text, open_zip, pair_map, rewrite_zip, WritebackReport};

/// epub 双语回写：见模块级文档。`pairs` 的 id 与
/// [`crate::emit::to_translation_units`] 同口径（`p{页}-b{块序}`，页名取
/// Document 页名）；`pair.source` 携带①侧源文本时写入前校验，不一致跳过并
/// 记入报告（宁漏勿错）。失败返回 `corrupt`。
pub fn writeback_epub(
    in_path: impl AsRef<Path>,
    pairs: &[crate::emit::TranslationPair],
    strategy: BiStrategy,
    out: impl AsRef<Path>,
) -> crate::error::Result<crate::writeback::WritebackReport> {
    let in_path = in_path.as_ref();
    let out = out.as_ref();
    let mut plan = plan_epub_edits(in_path, pairs, strategy)?;
    let mut report = std::mem::take(&mut plan.report);

    // F. 改写并重打包（非 UTF-8 条目不改写，防编码声明失配）
    let mut replaced: Vec<(String, Vec<u8>)> = Vec::new();
    let parts_len = plan.parts.len(); // iter_mut 借用期内不再借用 plan.parts
    for (i, part) in plan.parts.iter_mut().enumerate() {
        if plan.edits[i].is_empty() {
            continue;
        }
        if std::str::from_utf8(&part.bytes).is_err() {
            report.skip(part.name.clone(), "部件非 UTF-8（防编码声明失配），未改写");
            continue;
        }
        // written 只计实际落盘改写的部件（规划期逐单元累计；附录条目随最后
        // 部件的改写计入）
        report.written += plan.part_units[i];
        if strategy == BiStrategy::OriginalPlusAppendix && i + 1 == parts_len {
            report.written += plan.appendix_entries;
        }
        part.xhtml = apply_edits(&part.xhtml, std::mem::take(&mut plan.edits[i]));
        replaced.push((part.name.clone(), part.xhtml.clone().into_bytes()));
    }
    rewrite_zip(in_path, out, &replaced)?;
    Ok(report)
}

/// EPUB 部件级交替：spine 每个有命中译文的部件后插入「该部件的纯译文部件」
/// ——阅读器里即 真·一页原文一页译文（部件=章/页边界）。`pairs` 的
/// id 与 [`crate::emit::to_translation_units`] 同口径（源校验同
/// [`writeback_epub`]）；无任何命中单元（无实际编辑）的部件不插译文部件
/// （避免逐页「相同页翻两遍」）。OPF 非 UTF-8 返回 `corrupt`（译文部件的
/// 挂载骨架不可缺，见模块级文档）；其余失败同样返回 `corrupt`。
pub fn writeback_epub_alternating(
    in_path: impl AsRef<Path>,
    pairs: &[crate::emit::TranslationPair],
    out: impl AsRef<Path>,
) -> crate::error::Result<crate::writeback::WritebackReport> {
    let in_path = in_path.as_ref();
    let out = out.as_ref();

    // A–E 共用管线；策略固定 TranslationOnly——译文部件 = 该部件全部命中
    // 单元的纯译文渲染（未命中单元原样，部件仍是完整合法 XHTML）。
    let mut plan = plan_epub_edits(in_path, pairs, BiStrategy::TranslationOnly)?;
    let mut report = std::mem::take(&mut plan.report);

    // OPF 通道：包内路径 + 文本（非 UTF-8 整体拒绝，见函数头）。
    let (opf_name, opf_xml) = opf_channel(in_path)?;
    // 全 manifest id 的查重集合（撞名防御不限于 spine 部件之间）。
    let (items, _) = parse_opf_spine(&opf_xml)?;
    let mut used_ids: std::collections::HashSet<String> =
        items.into_iter().map(|(id, _)| id).collect();
    // OPF 插入点：`</manifest>` 前（新 item 统一挂这里）+ 各原部件
    // itemref 之后（译文 itemref 原位插入，翻页序即 原/译交替）。
    let points = opf_insert_points(&opf_xml, &plan.parts)?;

    let mut replaced: Vec<(String, Vec<u8>)> = Vec::new();
    let mut added: Vec<(String, Vec<u8>)> = Vec::new();
    let mut opf_edits: Vec<Edit> = Vec::new();
    let mut manifest_items = String::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (i, part) in plan.parts.iter_mut().enumerate() {
        let edits = std::mem::take(&mut plan.edits[i]);
        if edits.is_empty() {
            continue; // 无命中单元（无实际编辑）→ 不插译文部件（防「相同页翻两遍」）
        }
        if std::str::from_utf8(&part.bytes).is_err() {
            // 非 UTF-8 部件不改写（与 writeback_epub 同款守卫）：不插译文页
            report.skip(
                part.name.clone(),
                "部件非 UTF-8（防编码声明失配），未插译文部件",
            );
            continue;
        }
        let tr_name = tr_part_name(&part.name);
        // 同一部件在 spine 重复出现（EPUB3 违规形态）：只挂一次译文部件
        if !seen_names.insert(tr_name.clone()) {
            report.skip(part.name.clone(), "部件在 spine 重复出现，未插译文部件");
            continue;
        }
        report.written += plan.part_units[i];
        let tr_id = tr_manifest_id(&part.id, &mut used_ids);
        let tr_xhtml = apply_edits(&part.xhtml, edits);
        added.push((tr_name, tr_xhtml.into_bytes()));
        manifest_items.push_str(&format!(
            "<item id=\"{tr_id}\" href=\"{}\" media-type=\"application/xhtml+xml\"/>",
            tr_part_href(&part.href),
        ));
        let pos = points.itemrefs.get(&part.id).copied().ok_or_else(|| {
            DocError::corrupt(format!("OPF spine 缺部件 {} 的 itemref", part.id))
        })?;
        opf_edits.push(Edit::Insert(pos, format!("<itemref idref=\"{tr_id}\"/>")));
    }

    if added.is_empty() {
        // 全书无命中：输出 = 原包透传（OPF 也不动）
        return rewrite_zip(in_path, out, &[]).map(|_| report);
    }
    opf_edits.push(Edit::Insert(points.manifest, manifest_items));
    replaced.push((opf_name, apply_edits(&opf_xml, opf_edits).into_bytes()));
    rewrite_zip_added(in_path, out, &replaced, &added)?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// 回写共用前半（A–E）：两种回写形态的公共管线
// ---------------------------------------------------------------------------

/// [`plan_epub_edits`] 的产物：spine 部件通道 + 逐部件编辑 + 报告素材。
/// `writeback_epub` 拿它替换原部件字节；`writeback_epub_alternating` 拿它
/// 另立译文部件。
struct EpubPlan {
    /// spine 部件通道（id/href 供 alternating 派生译文部件的 OPF 条目）。
    parts: Vec<SpinePart>,
    /// 逐部件编辑（与 parts 同下标；空 = 该部件无命中/无编辑）。
    edits: Vec<Vec<Edit>>,
    /// 每部件「已产出编辑的单元数」（written 素材；部件实际改写 / 译文
    /// 部件实际插入时才计入报告——规划了编辑但没落盘不算写入）。
    part_units: Vec<usize>,
    /// 附录条目数（OriginalPlusAppendix；附录编辑落在最后部件，随其改写
    /// 计入 written）。
    appendix_entries: usize,
    /// 规划期记下的跳过（源校验/守卫/结构，配对处能对到单元 id 的记 id）；
    /// 收尾时并入最终报告。
    report: WritebackReport,
}

/// A–E 公共管线：权威解析 → spine 部件 → 重块化 → provenance → 按策略
/// 产逐部件编辑（含 OriginalPlusAppendix 的附录追加）。两种回写形态的
/// 差异全部在各自收尾（替换原部件字节 vs 另立译文部件 + OPF 编辑）。
/// 报告：源校验/守卫/结构跳过在配对处记入 `report`，written 素材按部件
/// 累计（收尾方决定哪些部件真正落盘）。
fn plan_epub_edits(
    in_path: &Path,
    pairs: &[crate::emit::TranslationPair],
    strategy: BiStrategy,
) -> Result<EpubPlan> {
    // A. 权威解析（提取侧同管线、同默认选项）拿 id / 文本
    let raw = EpubRawBackend::default().parse(in_path)?;
    let doc = EpubPlugin.assemble(&raw);
    // 单元流全量（含表格格单元 kind = Table）：块级候选按块序消费、表格格
    // 在表格位逐格消费（见主循环的 Table 分支），两段消费拼满同一单元流。
    let units = crate::emit::to_translation_units(&doc);
    let map = pair_map(pairs);
    let mut report = WritebackReport::default();
    let mut appendix_entries = 0usize;

    // B. spine 部件通道（与 EpubRawBackend::parse 的 spine 循环同款跳过规则）
    let parts = spine_parts(in_path)?;
    if parts.len() != raw.pages.len() {
        return Err(DocError::corrupt(
            "epub spine 对齐失败：部件通道与解析器页数不一致",
        ));
    }
    let mut part_units = vec![0usize; parts.len()];

    // C. 逐 spine 文档重块化（parse_blocks 同口径扫描，带 span）
    let mut slots: Vec<Vec<HtmlSlot>> = Vec::with_capacity(parts.len());
    let mut body_closes: Vec<Option<usize>> = Vec::with_capacity(parts.len());
    for part in &parts {
        let (s, bc) = scan_html_blocks(&part.xhtml);
        slots.push(s);
        body_closes.push(bc);
    }

    // D. Document 页 → raw 块 provenance（assemble 同口径）
    let prov = page_provenance(&raw, &slots)?;
    if prov.len() != doc.pages.len() {
        return Err(DocError::corrupt(
            "epub 对齐失败：provenance 页数与 Document 不一致",
        ));
    }

    // E. 单元 ↔（页, 页内块）与 units[] 严格同序（同一过滤口径），逐策略产编辑
    let mut edits: Vec<Vec<Edit>> = (0..parts.len()).map(|_| Vec::new()).collect();
    let mut cursor = 0usize;
    for (page_idx, page) in doc.pages.iter().enumerate() {
        let doc_i = prov[page_idx].doc;
        let raw_slots = &slots[doc_i];
        for (b_idx, b) in page.blocks.iter().enumerate() {
            if b.kind == BlockKind::Table {
                // 表格格级回写（wave2）：仅切节页有 Table 块（坍缩页的表格
                // 文本已并入段落单元，见模块头「单节坍缩边界」）。扫描格序列
                // ↔ 格单元顺序配对 + 文本守卫；OriginalPlusAppendix 表格原样
                // （格译文不进附录）。无 rows 结构的 Table 块防御性跳过。
                if b.table.is_none() {
                    continue;
                }
                let slot = &raw_slots[prov[page_idx].blocks[b_idx]];
                for cell in &slot.cells {
                    let Some(unit) = units.get(cursor) else { break };
                    cursor += 1;
                    // 译文查找先行：无译文/空白 = 正常未翻译，守卫不记报告
                    let Some((tr, src_opt)) = map.get(unit.id.as_str()) else {
                        continue;
                    };
                    let tr = tr.trim();
                    if tr.is_empty() {
                        continue;
                    }
                    // 双保险：格文本 ≠ 单元文本 → 该格不写（宁漏勿错；单元照常消费）
                    if unit.kind != BlockKind::Table || cell.text != unit.text {
                        report.skip(unit.id.clone(), "文本守卫不一致");
                        continue;
                    }
                    // ①→④ 边界：译文对携带的①源文本与当前(④)文本不一致 → 跳过并记报告
                    //（位置锚点在新文件里指向别的文本，写下去就是静默错位写）
                    if let Some(src) = src_opt {
                        if src.trim() != unit.text.trim() {
                            report.skip(unit.id.clone(), "源文本不一致（提取后文件已变更）");
                            continue;
                        }
                    }
                    let before = edits[doc_i].len();
                    match strategy {
                        BiStrategy::BilingualInline => {
                            // 格内容终点（闭标签前）插 <br class="docforge-tr"/> + 译文
                            edits[doc_i].push(Edit::Insert(
                                cell.end,
                                format!("<br class=\"{TR_CLASS}\"/>{}", escape_xml_text(tr)),
                            ));
                        }
                        BiStrategy::TranslationOnly => {
                            // 格内首文本节点换译文、其余清空（与段落同款）
                            for (k, (s, e)) in cell.text_spans.iter().enumerate() {
                                if k == 0 {
                                    edits[doc_i]
                                        .push(Edit::Replace(*s, *e, escape_xml_text(tr)));
                                } else {
                                    edits[doc_i].push(Edit::Replace(*s, *e, String::new()));
                                }
                            }
                        }
                        BiStrategy::OriginalPlusAppendix => {}
                    }
                    // 报告：产出编辑的格计 written；有译文但零编辑（格内无
                    // 文本节点）记结构跳过；附录策略格不在此写（附录集中处理）
                    if edits[doc_i].len() > before {
                        part_units[doc_i] += 1;
                    } else if strategy != BiStrategy::OriginalPlusAppendix {
                        report.skip(unit.id.clone(), "结构限制（格内无文本节点）");
                    }
                }
                continue;
            }
            let t = b.content.trim();
            if !crate::emit::is_translatable(b.kind) || t.is_empty() || t.chars().count() < 2 {
                continue;
            }
            let unit = &units[cursor];
            cursor += 1;
            // 译文查找先行：无译文/空白 = 正常未翻译，守卫不记报告
            let Some((tr, src_opt)) = map.get(unit.id.as_str()) else {
                continue;
            };
            let tr = tr.trim();
            if tr.is_empty() {
                continue;
            }
            // 单元 → 贡献 raw 块（切节页：单块；坍缩页：全部非空块）
            let (contrib, expect_text) = if prov[page_idx].collapsed {
                let idxs = prov[page_idx].blocks.clone();
                let joined = idxs
                    .iter()
                    .map(|&i| raw_slots[i].text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                (idxs, joined)
            } else {
                let i = prov[page_idx].blocks[b_idx];
                (vec![i], raw_slots[i].text.clone())
            };
            // 双保险：重块化文本必须与权威单元文本一致
            if expect_text != unit.text {
                report.skip(unit.id.clone(), "文本守卫不一致");
                continue;
            }
            // ①→④ 边界：译文对携带的①源文本与当前(④)文本不一致 → 跳过并记报告
            //（位置锚点在新文件里指向别的文本，写下去就是静默错位写）
            if let Some(src) = src_opt {
                if src.trim() != unit.text.trim() {
                    report.skip(unit.id.clone(), "源文本不一致（提取后文件已变更）");
                    continue;
                }
            }
            let before = edits[doc_i].len();
            match strategy {
                BiStrategy::BilingualInline => {
                    // 插在最后一块之后；无块回退的全文 span 钳进 </body> 前
                    let mut pos = raw_slots[contrib[contrib.len() - 1]].end;
                    if let Some(bc) = body_closes[doc_i] {
                        pos = pos.min(bc);
                    }
                    edits[doc_i].push(Edit::Insert(
                        pos,
                        format!("<p class=\"{TR_CLASS}\">{}</p>", escape_xml_text(tr)),
                    ));
                }
                BiStrategy::TranslationOnly => {
                    // 首个贡献块的首文本节点换译文；其余可翻译块的文本节点清空。
                    // Image/Code 结构块的文本不动；坍缩页贡献块中的 Table 槽位
                    // 同样不动（坍缩页无格单元，见模块头「单节坍缩边界」）。
                    let mut first = true;
                    for i in &contrib {
                        let slot = &raw_slots[*i];
                        if !matches!(
                            slot.kind,
                            BlockKind::Heading
                                | BlockKind::Paragraph
                                | BlockKind::Quote
                                | BlockKind::List
                        ) {
                            continue;
                        }
                        for (s, e) in &slot.text_spans {
                            if first {
                                edits[doc_i].push(Edit::Replace(*s, *e, escape_xml_text(tr)));
                                first = false;
                            } else {
                                edits[doc_i].push(Edit::Replace(*s, *e, String::new()));
                            }
                        }
                    }
                }
                BiStrategy::OriginalPlusAppendix => {
                    // 末尾统一追加（见下）
                }
            }
            // 报告：产出编辑的单元计 written；有译文但零编辑（贡献块皆无
            // 可写文本位）记结构跳过；附录策略单元在附录处集中计入
            if edits[doc_i].len() > before {
                part_units[doc_i] += 1;
            } else if strategy != BiStrategy::OriginalPlusAppendix {
                report.skip(unit.id.clone(), "结构限制（无可写文本位）");
            }
        }
    }

    // Appendix：正文不动，最后 spine 文档 `</body>` 前追加「译文」小节
    if strategy == BiStrategy::OriginalPlusAppendix && !units.is_empty() && !parts.is_empty() {
        let mut matched: Vec<&str> = Vec::new();
        for u in units.iter() {
            let Some((t, _src)) = map.get(u.id.as_str()) else {
                continue;
            };
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            // 表格格译文不进附录（附录是段落流；表格正文原样）——有译文
            // 但未写入，记跳过
            if u.kind == BlockKind::Table {
                report.skip(u.id.clone(), "结构限制（附录不含表格）");
            } else {
                matched.push(t);
            }
        }
        if !matched.is_empty() {
            appendix_entries = matched.len();
            let doc_i = parts.len() - 1;
            let mut appendix = String::from("<h2>译文</h2>");
            for tr in matched {
                appendix.push_str(&format!("<p>{}</p>", escape_xml_text(tr)));
            }
            let pos = body_closes[doc_i].unwrap_or(parts[doc_i].xhtml.len());
            edits[doc_i].push(Edit::Insert(pos, appendix));
        }
    }

    Ok(EpubPlan {
        parts,
        edits,
        part_units,
        appendix_entries,
        report,
    })
}

// ---------------------------------------------------------------------------
// 译文部件命名 / id 派生（alternating）
// ---------------------------------------------------------------------------

/// 译文部件包内名：`{原部件名去扩展}.tr.xhtml`（与原部件同目录——原部件
/// head 里 `<link>` 的相对 href 天然不变，无相对路径重算）。
fn tr_part_name(name: &str) -> String {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    format!("{stem}.tr.xhtml")
}

/// 译文部件的 manifest href：原 href 换末段文件名（同目录命名 ⇒ 目录段
/// 原样）。与 [`tr_part_name`] 自洽：`resolve(opf_dir, tr_href)` 恰等于
/// `tr_part_name(resolve(opf_dir, href))`（resolve 逐段归一路径，末段文件
/// 名原样拼接）。
fn tr_part_href(href: &str) -> String {
    let href = href.split('#').next().unwrap_or(href);
    match href.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/{}", tr_part_name(file)),
        None => tr_part_name(href),
    }
}

/// 译文 manifest id：`{原 id}-tr`；已被占用则 `-tr2`、`-tr3`…（手工件 /
/// 重复处理的书撞名防御）。新 id 即刻入集合——同书多次派生互不撞名。
fn tr_manifest_id(orig: &str, used: &mut std::collections::HashSet<String>) -> String {
    let mut n = 1usize;
    loop {
        let cand = if n == 1 {
            format!("{orig}-tr")
        } else {
            format!("{orig}-tr{n}")
        };
        if !used.contains(&cand) {
            used.insert(cand.clone());
            return cand;
        }
        n += 1;
    }
}

// ---------------------------------------------------------------------------
// Document 页 → raw 块 provenance（assemble_impl 同口径的复刻）
// ---------------------------------------------------------------------------

/// 一个 Document 页的归属：来自哪个 spine 文档、覆盖哪些 raw 块。
struct PageProv {
    /// 所属 spine 文档下标（= raw.pages / parts 下标）。
    doc: usize,
    /// 是否单节坍缩页（Document 页只有一段合并文本）。
    collapsed: bool,
    /// 页对应的 raw 块下标（坍缩页 = 全部非空块；切节页 = 该节的块下标序列）。
    blocks: Vec<usize>,
}

/// 复刻 `EpubPlugin::assemble_impl` 的分页判定：纯图/空白页不成章；
/// `sections`（层级自适应切节）多节 → 每节一页；单节 → 坍缩页。
fn page_provenance(raw: &RawDoc, slots: &[Vec<HtmlSlot>]) -> Result<Vec<PageProv>> {
    let mut out = Vec::new();
    for (doc_i, rp) in raw.pages.iter().enumerate() {
        let Some(_xhtml) = rp.items.first().map(|i| i.text.as_str()) else {
            continue;
        };
        let raw_slots = &slots[doc_i];
        if raw_slots.iter().all(|s| s.text.is_empty()) {
            continue; // 纯图/空白页不成章（与 assemble 同口径）
        }
        // 复用提取侧的切节函数（层级自适应 h2→h3→h4 必须同源）
        let blocks_for_cut: Vec<Block> = raw_slots
            .iter()
            .map(|s| Block {
                kind: s.kind,
                content: s.text.clone(),
                level: s.level,
                ..Default::default()
            })
            .collect();
        let secs = crate::backends::epub::sections(&blocks_for_cut);
        if secs.len() > 1 {
            for (_title, idxs) in secs {
                out.push(PageProv {
                    doc: doc_i,
                    collapsed: false,
                    blocks: idxs,
                });
            }
            continue;
        }
        out.push(PageProv {
            doc: doc_i,
            collapsed: true,
            blocks: (0..raw_slots.len())
                .filter(|&i| !raw_slots[i].text.is_empty())
                .collect(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// spine 部件通道（镜像 backends/epub.rs 的容器→OPF→spine 循环）
// ---------------------------------------------------------------------------

struct SpinePart {
    /// 包内路径（ZIP 条目名）。
    name: String,
    /// 原始字节（UTF-8 判定用）。
    bytes: Vec<u8>,
    /// 与提取侧 `decode_entry` 同口径解码出的 XHTML。
    xhtml: String,
    /// manifest id（= spine idref；alternating 的译文 id 派生基）。
    id: String,
    /// manifest href（OPF 目录相对路径，未 resolve；alternating 的译文
    /// href 派生基）。
    href: String,
}

/// 条目解码（镜像 `backends/epub.rs::decode_entry`）：
/// UTF-8 直取 → XML 声明嗅探（`encoding` feature 下 encoding_rs）→ 统一解码链兜底。
fn decode_entry(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let head = &bytes[..bytes.len().min(1024)];
    let head_lossy = String::from_utf8_lossy(head);
    if let Some(enc) = head_lossy
        .split("encoding")
        .nth(1)
        .and_then(|rest| {
            rest.trim_start()
                .strip_prefix('=')
                .map(|r| r.trim_start().trim_start_matches('"').trim_start_matches('\''))
        })
        .and_then(|r| r.split(['"', '\'', ' ', '?']).next())
        .map(str::to_ascii_lowercase)
    {
        #[cfg(feature = "encoding")]
        {
            for label in [&enc, &enc.replace('_', "-")] {
                if let Some(cs) = encoding_rs::Encoding::for_label(label.as_bytes()) {
                    if cs == encoding_rs::UTF_8 {
                        break;
                    }
                    let (s, _, had_err) = cs.decode(bytes);
                    if !had_err {
                        return s.into_owned();
                    }
                }
            }
        }
        let _ = enc;
    }
    crate::encoding::decode(bytes.to_vec())
}

fn read_entry_bytes(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    match zip.by_name(name) {
        Ok(mut f) => {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut buf)
                .map_err(|e| DocError::corrupt(format!("读取 {name} 失败: {e}")))?;
            Ok(Some(buf))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(DocError::corrupt(format!("ZIP 读取 {name} 失败: {e}"))),
    }
}

/// container.xml → rootfile 的 OPF 包内路径（spine_parts 与 opf_channel
/// 共用的容器定位头）。
fn root_opf_path(zip: &mut zip::ZipArchive<std::fs::File>) -> Result<String> {
    let container = read_entry_bytes(zip, "META-INF/container.xml")?
        .map(|b| decode_entry(&b))
        .ok_or_else(|| DocError::corrupt("EPUB 缺 META-INF/container.xml"))?;
    crate::backends::epub::find_rootfile(&container)?
        .ok_or_else(|| DocError::corrupt("container.xml 缺 rootfile"))
}

/// OPF 通道（alternating 专用）：包内路径 + 解码文本。OPF 非 UTF-8 → Err：
/// OPF 是全部译文部件的挂载骨架，静默跳过会产出「有条目无挂载」的哑书，
/// 这里宁响勿哑（EPUB OCF 规范本就要求 OPF 用 UTF-8/UTF-16）。
fn opf_channel(path: &Path) -> Result<(String, String)> {
    let mut zip = open_zip(path)?;
    let opf_path = root_opf_path(&mut zip)?;
    let bytes = read_entry_bytes(&mut zip, &opf_path)?
        .ok_or_else(|| DocError::corrupt(format!("EPUB 缺 OPF: {opf_path}")))?;
    if std::str::from_utf8(&bytes).is_err() {
        return Err(DocError::corrupt(format!("EPUB OPF 非 UTF-8: {opf_path}")));
    }
    Ok((opf_path, decode_entry(&bytes)))
}

/// spine 阅读序的部件列表：linear="no" / manifest 缺项 / .ncx / 缺条目一律跳过
/// （与 `EpubRawBackend::parse` 同款规则，保证与 raw.pages 一一同位）。
fn spine_parts(path: &Path) -> Result<Vec<SpinePart>> {
    let mut zip = open_zip(path)?;
    let opf_path = root_opf_path(&mut zip)?;
    let opf_xml = read_entry_bytes(&mut zip, &opf_path)?
        .map(|b| decode_entry(&b))
        .ok_or_else(|| DocError::corrupt(format!("EPUB 缺 OPF: {opf_path}")))?;
    let (items, spine) = parse_opf_spine(&opf_xml)?;
    let opf_dir = opf_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    let mut parts = Vec::new();
    for (idref, linear) in spine {
        if !linear {
            continue;
        }
        let Some(href) = items.iter().find(|(id, _)| *id == idref).map(|(_, h)| h) else {
            continue;
        };
        let full = crate::backends::epub::resolve(opf_dir, href);
        if full.ends_with(".ncx") {
            continue;
        }
        let Some(bytes) = read_entry_bytes(&mut zip, &full)? else {
            continue;
        };
        parts.push(SpinePart {
            name: full,
            bytes: bytes.clone(),
            xhtml: decode_entry(&bytes),
            id: idref,
            href: href.clone(),
        });
    }
    Ok(parts)
}

/// 元素某属性的原始值（未反转义——回写侧把这些值原样拼回 XML，已转义
/// 形态保持合法；quick-xml 的 `value` 即原始字节）。
fn xml_attr(e: &quick_xml::events::BytesStart, k: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == k)
        .map(|a| a.value.to_string())
}

/// OPF 只取 manifest `(id, href)` 与 spine `(idref, linear)`（导航/封面与回写无关）。
#[allow(clippy::type_complexity)]
fn parse_opf_spine(xml: &str) -> Result<(Vec<(String, String)>, Vec<(String, bool)>)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut items = Vec::new();
    let mut spine = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.local_name().as_ref().to_string();
                match name.as_str() {
                    "item" => {
                        if let (Some(id), Some(href)) = (xml_attr(e, "id"), xml_attr(e, "href")) {
                            items.push((id, href));
                        }
                    }
                    "itemref" => {
                        if let Some(idref) = xml_attr(e, "idref") {
                            let linear = xml_attr(e, "linear").map(|v| v != "no").unwrap_or(true);
                            spine.push((idref, linear));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("OPF 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok((items, spine))
}

/// OPF 插入点扫描：`</manifest>` 之前（新 `<item>` 的统一插入位）+ 各
/// spine 部件 `<itemref>` 元素之后（译文 itemref 的原位插入位）。
struct OpfInsertPoints {
    /// `</manifest>` 的 `<` 偏移（manifest 条目统一插这里）。
    manifest: usize,
    /// 部件 id → 其 itemref 元素终点（闭标签 `>` 之后）。
    itemrefs: std::collections::HashMap<String, usize>,
}

/// 一次 quick_xml 扫描拿全部插入位（`buffer_position` = 刚读完的事件终点，
/// 对 `Reader::from_str` 即源串字节偏移）。itemref 覆盖自闭合与
/// Start/End 对两种形态；manifest 取 End 事件终点回退闭标签长度——
/// `e.name()` 是含前缀的原始名，长度精确（XML 闭标签不可能带属性）。
fn opf_insert_points(opf_xml: &str, parts: &[SpinePart]) -> Result<OpfInsertPoints> {
    let mut reader = Reader::from_str(opf_xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let wanted: std::collections::HashSet<&str> = parts.iter().map(|p| p.id.as_str()).collect();
    let mut itemrefs = std::collections::HashMap::new();
    let mut manifest = None;
    let mut pending: Option<String> = None; // Start 形态 itemref：等它的 End
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.local_name().as_ref() == "itemref" {
                    if let Some(id) =
                        xml_attr(e, "idref").filter(|id| wanted.contains(id.as_str()))
                    {
                        pending = Some(id);
                    }
                }
            }
            Ok(Event::Empty(ref e)) => {
                if e.local_name().as_ref() == "itemref" {
                    if let Some(id) =
                        xml_attr(e, "idref").filter(|id| wanted.contains(id.as_str()))
                    {
                        itemrefs.insert(id, reader.buffer_position() as usize);
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let raw = e.name();
                if raw.local_name().as_ref() == "itemref" {
                    if let Some(id) = pending.take() {
                        itemrefs.insert(id, reader.buffer_position() as usize);
                    }
                } else if raw.local_name().as_ref() == "manifest" {
                    manifest =
                        Some(reader.buffer_position() as usize - (raw.as_ref().len() + 3));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("OPF 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(OpfInsertPoints {
        manifest: manifest.ok_or_else(|| DocError::corrupt("OPF 缺 </manifest> 闭标签"))?,
        itemrefs,
    })
}

/// [`rewrite_zip`] 的带新增变体（alternating 专用）：`replaced` 换既有条目
/// 字节、`added` 追加新条目。共用件的 replaced 机制按名匹配既有条目、天然
/// 只换不加（包外名字静默丢弃），译文部件是新条目，故在本文件内镜像一份
/// 加 `added` 参数——mimetype 首条目 + Stored、既有条目原序透传等约定与
/// zip_util::rewrite_zip 一致；added 追加在末尾（EPUB 不约束条目物理序）。
fn rewrite_zip_added(
    in_path: &Path,
    out_path: &Path,
    replaced: &[(String, Vec<u8>)],
    added: &[(String, Vec<u8>)],
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
        std::io::Read::read_to_end(&mut f, &mut data)
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
        std::io::Write::write_all(&mut zw, data)
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
        std::io::Write::write_all(&mut zw, &data)
            .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
    }
    for (name, data) in added {
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file(name.as_str(), opts)
            .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
        std::io::Write::write_all(&mut zw, data)
            .map_err(|e| DocError::corrupt(format!("ZIP 写 {name} 失败: {e}")))?;
    }
    zw.finish()
        .map_err(|e| DocError::corrupt(format!("ZIP 收尾失败: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// XHTML 候选位扫描：已抽到 writeback::html 共用（本文件逐 spine 文档调用，
// html 单文件回写对整篇调用一次）——见 super::html::scan_html_blocks / HtmlSlot。
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::path::PathBuf;

    use crate::backends::epub::read_epub;
    use crate::emit::to_translation_units;
    use crate::model::BlockKind::{Heading, List, Paragraph, Table};

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    const NAV: &str = r#"<?xml version="1.0"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>
<nav epub:type="toc" xmlns:epub="http://www.idpf.org/2007/ops">
  <ol>
    <li><a href="text/ch1.xhtml">第一章</a></li>
    <li><a href="text/ch2.xhtml">第二章</a></li>
  </ol>
</nav></body></html>"#;

    /// ch1：单节文档（坍缩页）；含表格——坍缩形态下表格文本并入段落单元、
    /// 格单元不存在（见模块头「单节坍缩边界」）。
    const CH1: &str = "<html><body><h1>第一章</h1><p>正文一。</p><p>正文二。</p>\
<table><tr><td>格甲</td><td>格乙</td></tr></table></body></html>";
    /// ch2：两个 h2 → 切节页。
    const CH2: &str = "<html><body><h2>甲节</h2><p>甲正文，段落测试。</p>\
<h2>乙节</h2><ul><li>要点一。</li><li>要点二。</li></ul></body></html>";
    /// ch3：两个 h2 → 切节页；首节含表格（格级回写目标，colspan 行无展开）。
    const CH3: &str = "<html><body><h2>参数节</h2><p>如下表所示。</p>\
<table><tr><th>名称</th><th>单位</th></tr>\
<tr><td>电压</td><td>伏</td></tr>\
<tr><td colspan=\"2\">跨列说明</td></tr></table>\
<h2>结论节</h2><p>综上各条。</p></body></html>";
    /// ch1/ch2 带 head 样式表的变体（alternating 的 CSS 同目录收益断言；
    /// head 在 SKIP_TAGS，块流与单元 id 与 CH1/CH2 完全同口径）。样式表在
    /// OEBPS/style.css，章文件在 OEBPS/text/ ⇒ 相对 href = ../style.css。
    const CH1_CSS: &str = "<html><head><title>c1</title>\
<link rel=\"stylesheet\" type=\"text/css\" href=\"../style.css\"/></head>\
<body><h1>第一章</h1><p>正文一。</p><p>正文二。</p></body></html>";
    const CH2_CSS: &str = "<html><head><title>c2</title>\
<link rel=\"stylesheet\" type=\"text/css\" href=\"../style.css\"/></head>\
<body><h2>甲节</h2><p>甲正文，段落测试。</p>\
<h2>乙节</h2><ul><li>要点一。</li><li>要点二。</li></ul></body></html>";
    /// 样式表条目（build_epub_parts_in 的 extra 注入件，配 CH*_CSS 用）。
    const CSS_ITEM: &str = r#"<item id="css" href="style.css" media-type="text/css"/>"#;
    const STYLE_CSS: &str = "body { font-family: serif; }";

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_epub_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// N 章 epub：manifest/spine 按 chs 动态生成（ch{i} → text/ch{i}.xhtml；
    /// 章名 nav 只给前两章，后续章页名走文档内标题口径）。
    fn build_epub_parts(dir: &Path, chs: &[&str]) -> PathBuf {
        build_epub_parts_in(dir, chs, "", &[])
    }

    /// [`build_epub_parts`] 的带注入变体（alternating 测试用）：`extra_manifest`
    /// 追加 manifest 条目（样式表 / id 撞名防御件），`extra_files` 追加
    /// ZIP 条目。
    fn build_epub_parts_in(
        dir: &Path,
        chs: &[&str],
        extra_manifest: &str,
        extra_files: &[(&str, &str)],
    ) -> PathBuf {
        let path = dir.join("book.epub");
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        let mut put = |name: &str, body: &str| {
            zw.start_file(name, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        };
        let mut items = (1..=chs.len())
            .map(|i| {
                format!(
                    r#"<item id="ch{i}" href="text/ch{i}.xhtml" media-type="application/xhtml+xml"/>"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n    ");
        if !extra_manifest.is_empty() {
            items.push_str("\n    ");
            items.push_str(extra_manifest);
        }
        let spine = (1..=chs.len())
            .map(|i| format!(r#"<itemref idref="ch{i}"/>"#))
            .collect::<Vec<_>>()
            .join("\n    ");
        let opf = format!(
            r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>测试书</dc:title>
    <dc:language>zh</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    {items}
  </manifest>
  <spine>
    {spine}
  </spine>
</package>"#
        );
        put("mimetype", "application/epub+zip");
        put("META-INF/container.xml", CONTAINER);
        put("OEBPS/content.opf", &opf);
        put("OEBPS/nav.xhtml", NAV);
        for (i, ch) in chs.iter().enumerate() {
            put(&format!("OEBPS/text/ch{}.xhtml", i + 1), ch);
        }
        for (name, body) in extra_files {
            put(name, body);
        }
        zw.finish().unwrap();
        path
    }

    fn build_epub(dir: &Path, ch1: &str, ch2: &str) -> PathBuf {
        build_epub_parts(dir, &[ch1, ch2])
    }

    /// 提取侧同管线（EpubRawBackend + EpubPlugin::assemble）。
    fn parse_doc(path: &Path) -> crate::model::Document {
        let raw = EpubRawBackend::default()
            .parse(path)
            .unwrap();
        EpubPlugin.assemble(&raw)
    }

    fn unit_ids(path: &Path) -> Vec<String> {
        to_translation_units(&parse_doc(path))
            .into_iter()
            .map(|u| u.id)
            .collect()
    }

    fn write(
        in_path: &Path,
        out_path: &Path,
        tr: &[(&str, &str)],
        strategy: BiStrategy,
    ) -> WritebackReport {
        let pairs: Vec<crate::emit::TranslationPair> = tr
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()).into())
            .collect();
        writeback_epub(in_path, &pairs, strategy, out_path).unwrap()
    }

    /// alternating 便捷入口（与 write 同款）。
    fn alternating_write(in_path: &Path, out_path: &Path, tr: &[(&str, &str)]) -> WritebackReport {
        let pairs: Vec<crate::emit::TranslationPair> = tr
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()).into())
            .collect();
        writeback_epub_alternating(in_path, &pairs, out_path).unwrap()
    }

    /// 读 ZIP 条目为字符串（UTF-8 断言件）。
    fn zip_entry_string(path: &Path, name: &str) -> String {
        let mut zip = open_zip(path).unwrap();
        let mut s = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    /// ZIP 是否含某条目名。
    fn zip_has_entry(path: &Path, name: &str) -> bool {
        let mut zip = open_zip(path).unwrap();
        (0..zip.len()).any(|i| zip.by_index(i).unwrap().name().unwrap() == name)
    }

    /// OPF spine 的 itemref idref 序（alternating 的翻页序断言）。
    fn spine_idrefs(opf: &str) -> Vec<String> {
        let mut reader = Reader::from_str(opf);
        reader.config_mut().trim_text(true);
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                    if e.local_name().as_ref() == "itemref" {
                        if let Some(id) = xml_attr(e, "idref") {
                            out.push(id);
                        }
                    }
                }
                Ok(Event::Eof) => break,
                _ => {}
            }
            buf.clear();
        }
        out
    }

    /// 把某条目字节整体换掉（OPF 非 UTF-8 拒绝测试的畸形书构造）。
    fn replace_zip_entry(in_path: &Path, out_path: &Path, name: &str, data: &[u8]) {
        let mut zin = open_zip(in_path).unwrap();
        let f = std::fs::File::create(out_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for i in 0..zin.len() {
            let mut f = zin.by_index(i).unwrap();
            let n = f.name().unwrap().to_string();
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).unwrap();
            zw.start_file(n.as_str(), opts).unwrap();
            if n == name {
                zw.write_all(data).unwrap();
            } else {
                zw.write_all(&buf).unwrap();
            }
        }
        zw.finish().unwrap();
    }

    /// 金测（核心验收）：BilingualInline 往返 —— 坍缩页插在最后非空块后、
    /// 切节页逐块原/译交错、表格不受扰、未匹配 id 忽略、mimetype 仍居首。
    #[test]
    fn roundtrip_bilingual_inline() {
        let dir = case_dir("inline");
        let in_path = build_epub(&dir, CH1, CH2);
        let out_path = dir.join("out.epub");

        // 提取侧 id 口径（页名 = 章名；乙节两个 li 各一单元）
        assert_eq!(
            unit_ids(&in_path),
            ["p第一章-b0", "p甲节-b0", "p乙节-b0", "p乙节-b1"]
        );

        write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
                ("p乙节-b1", "Point two."),
                ("p不存在-b0", "bogus"), // 未匹配 id：忽略
            ],
            BiStrategy::BilingualInline,
        );

        // 书 API（保留结构化块）：原/译交错
        let book = read_epub(&out_path).unwrap();
        let titles: Vec<&str> = book.chapters.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["第一章", "甲节", "乙节"]);

        let ch1 = &book.chapters[0].blocks;
        let kinds: Vec<BlockKind> = ch1.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, [Heading, Paragraph, Paragraph, Table, Paragraph]);
        assert_eq!(ch1[3].content, "格甲 | 格乙", "表格不受扰");
        assert_eq!(
            ch1[3].table.as_ref().unwrap().rows,
            vec![vec!["格甲".to_string(), "格乙".to_string()]]
        );
        assert_eq!(ch1[4].content, "Chapter 1", "坍缩页译文插在最后非空块（表格）后");

        let jia = &book.chapters[1].blocks;
        assert_eq!(
            jia.iter().map(|b| b.content.as_str()).collect::<Vec<_>>(),
            ["甲正文，段落测试。", "Section A body."]
        );
        let yi = &book.chapters[2].blocks;
        assert_eq!(
            yi.iter().map(|b| (b.kind, b.content.as_str())).collect::<Vec<_>>(),
            [
                (List, "要点一。"),
                (Paragraph, "Point one."),
                (List, "要点二。"),
                (Paragraph, "Point two."),
            ]
        );

        // mimetype 仍为首条目且 Stored；条目数不变
        let mut zin = open_zip(&in_path).unwrap();
        let mut zout = open_zip(&out_path).unwrap();
        assert_eq!(zout.len(), zin.len());
        assert_eq!(zout.by_index(0).unwrap().name().unwrap(), "mimetype");
        assert_eq!(
            zout.by_index(0).unwrap().compression(),
            zip::CompressionMethod::Stored
        );
        // 未改写的条目字节透传（nav.xhtml）
        let mut a = String::new();
        let mut b = String::new();
        zin.by_name("OEBPS/nav.xhtml")
            .unwrap()
            .read_to_string(&mut a)
            .unwrap();
        zout.by_name("OEBPS/nav.xhtml")
            .unwrap()
            .read_to_string(&mut b)
            .unwrap();
        assert_eq!(a, b);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TranslationOnly：坍缩页首块换译文、其余可翻译块清空（Document 口径：
    /// 页文本 = 译文）；切节页逐块替换；未翻译单元原样。
    #[test]
    fn translation_only_replaces_text() {
        let dir = case_dir("tonly");
        // ch1 不放表格：坍缩单元的全部贡献块都是可翻译块（简化口径的干净路径）
        let ch1 = "<html><body><h1>第一章</h1><p>正文一。</p><p>正文二。</p></body></html>";
        let in_path = build_epub(&dir, ch1, CH2);
        let out_path = dir.join("out.epub");
        write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
            ],
            BiStrategy::TranslationOnly,
        );

        let doc = parse_doc(&out_path);
        assert_eq!(doc.pages.len(), 3);
        // 坍缩页：h1 换译文、两个 p 清空 → 页块 = 单段译文
        assert_eq!(doc.pages[0].name.as_deref(), Some("第一章"));
        assert_eq!(doc.pages[0].blocks.len(), 1);
        assert_eq!(doc.pages[0].blocks[0].content, "Chapter 1");
        // 切节页：逐块替换
        assert_eq!(doc.pages[1].blocks[0].content, "Section A body.");
        assert_eq!(doc.pages[2].blocks[0].content, "Point one.");
        assert_eq!(doc.pages[2].blocks[1].content, "要点二。", "未翻译单元原样");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全部 id 未匹配：输出与输入的 Document 解析逐块一致（透传）。
    #[test]
    fn unmatched_ids_leave_book_untouched() {
        let dir = case_dir("unmatched");
        let in_path = build_epub(&dir, CH1, CH2);
        let out_path = dir.join("out.epub");
        write(
            &in_path,
            &out_path,
            &[("p不存在-b9", "nope")],
            BiStrategy::BilingualInline,
        );
        assert_eq!(parse_doc(&out_path), parse_doc(&in_path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OriginalPlusAppendix：正文不动，最后 spine 文档 </body> 前追加
    /// 「译文」小节（h2 标题 + 全部译文段，单元序）。
    #[test]
    fn appendix_appends_to_last_spine_doc() {
        let dir = case_dir("appendix");
        let in_path = build_epub(&dir, CH1, CH2);
        let out_path = dir.join("out.epub");
        write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
                ("p乙节-b1", "Point two."),
            ],
            BiStrategy::OriginalPlusAppendix,
        );

        // 前两个 spine 文档不动
        let raw_in = EpubRawBackend::default()
            .parse(&in_path)
            .unwrap();
        let raw_out = EpubRawBackend::default()
            .parse(&out_path)
            .unwrap();
        assert_eq!(raw_in.pages[0].items[0].text, raw_out.pages[0].items[0].text);

        // ch2 末尾追加 h2「译文」+ 4 段
        let ch2_out = &raw_out.pages[1].items[0].text;
        assert!(ch2_out.contains("<h2>译文</h2>"));
        for tr in ["Chapter 1", "Section A body.", "Point one.", "Point two."] {
            assert!(ch2_out.contains(&format!("<p>{tr}</p>")), "{tr} in appendix");
        }
        assert!(ch2_out.contains("要点一。"), "正文不动");
        let body_end = ch2_out.rfind("</body>").unwrap();
        let h2 = ch2_out.find("<h2>译文</h2>").unwrap();
        assert!(h2 < body_end, "附录在 </body> 之前");
        // 重解析：原三页内容不变；追加的 h2「译文」自开新节 → 第 4 页
        let doc = parse_doc(&out_path);
        assert_eq!(doc.pages.len(), 4);
        assert_eq!(doc.pages[0].blocks[0].content, "第一章\n\n正文一。\n\n正文二。\n\n格甲 | 格乙");
        assert_eq!(doc.pages[1].blocks[0].content, "甲正文，段落测试。");
        assert_eq!(doc.pages[2].blocks[0].content, "要点一。");
        assert_eq!(doc.pages[2].blocks[1].content, "要点二。");
        assert_eq!(doc.pages[3].name.as_deref(), Some("译文"));
        assert_eq!(
            doc.pages[3]
                .blocks
                .iter()
                .map(|b| b.content.as_str())
                .collect::<Vec<_>>(),
            ["Chapter 1", "Section A body.", "Point one.", "Point two."]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 表格格级回写（wave2 金测，切节页）：TranslationOnly 全格替换——格单元
    /// 全译后重解析，格文本 == 译文；colspan 行无展开（跨列文本落 (r,0)、
    /// 无续位）；单节坍缩页（ch1）不产格单元，表格文本随坍缩段原样。
    #[test]
    fn table_cells_translation_only_sectioned() {
        let dir = case_dir("cell-tonly");
        let in_path = build_epub_parts(&dir, &[CH1, CH2, CH3]);
        let out_path = dir.join("out.epub");

        // ch1 坍缩：无格单元（坍缩边界）；ch3 切节：表格按格展开，
        // 空格不发单元、单字符格照收、colspan 行只占 (r,0)
        assert_eq!(
            unit_ids(&in_path),
            [
                "p第一章-b0",
                "p甲节-b0",
                "p乙节-b0",
                "p乙节-b1",
                "p参数节-b0",
                "p参数节-b1-r0c0",
                "p参数节-b1-r0c1",
                "p参数节-b1-r1c0",
                "p参数节-b1-r1c1",
                "p参数节-b1-r2c0",
                "p结论节-b0",
            ]
        );

        write(
            &in_path,
            &out_path,
            &[
                ("p参数节-b0", "See the table."),
                ("p参数节-b1-r0c0", "Name"),
                ("p参数节-b1-r0c1", "Unit"),
                ("p参数节-b1-r1c0", "Voltage"),
                ("p参数节-b1-r1c1", "V"),
                ("p参数节-b1-r2c0", "Spanning note"),
                ("p结论节-b0", "As shown above."),
            ],
            BiStrategy::TranslationOnly,
        );

        let doc = parse_doc(&out_path);
        assert_eq!(doc.pages.len(), 5);
        // ch1 坍缩页：未喂译文，表格文本随坍缩段原样
        assert_eq!(
            doc.pages[0].blocks[0].content,
            "第一章\n\n正文一。\n\n正文二。\n\n格甲 | 格乙"
        );
        // 参数节（切节页）：段落替换 + 表格格全替换（colspan 行落 (r,0) 单格）
        assert_eq!(doc.pages[3].blocks[0].content, "See the table.");
        let rows = &doc.pages[3].blocks[1].table.as_ref().unwrap().rows;
        assert_eq!(
            rows,
            &vec![
                vec!["Name".to_string(), "Unit".to_string()],
                vec!["Voltage".to_string(), "V".to_string()],
                vec!["Spanning note".to_string()],
            ]
        );
        assert_eq!(doc.pages[4].blocks[0].content, "As shown above.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BilingualInline 格内对照 + OriginalPlusAppendix 附录排除（切节页）：
    /// 格内 `<br class="docforge-tr"/>` 分隔原/译（重解析为原/译直连拼接）、
    /// 未命中格原样；格译文不进附录（附录只收段落类）、表格原样。
    #[test]
    fn table_cells_inline_and_appendix_sectioned() {
        let dir = case_dir("cell-inline");
        let in_path = build_epub_parts(&dir, &[CH1, CH2, CH3]);

        // BilingualInline：只给电压格译文 → 该格 br 对照，其余格原样
        let out_path = dir.join("inline.epub");
        write(
            &in_path,
            &out_path,
            &[("p参数节-b1-r1c0", "Voltage")],
            BiStrategy::BilingualInline,
        );
        let raw_out = EpubRawBackend::default()
            .parse(&out_path)
            .unwrap();
        let ch3 = &raw_out.pages[2].items[0].text;
        assert!(
            ch3.contains(r#"<td>电压<br class="docforge-tr"/>Voltage</td>"#),
            "格内 br 对照插在格内容终点：{ch3}"
        );
        assert!(ch3.contains("<td>伏</td>"), "未命中格原样：{ch3}");
        let book = read_epub(&out_path).unwrap();
        let tbl = book.chapters[3].blocks[1].table.as_ref().unwrap();
        assert_eq!(
            tbl.rows[1],
            vec!["电压Voltage".to_string(), "伏".to_string()],
            "重解析：格文本原/译直连（br 不拆格）"
        );
        assert_eq!(tbl.rows[0], vec!["名称".to_string(), "单位".to_string()]);

        // OriginalPlusAppendix：格译文不进附录（附录只收段落类），表格原样
        let out2 = dir.join("appendix.epub");
        write(
            &in_path,
            &out2,
            &[
                ("p参数节-b0", "See the table."),
                ("p参数节-b1-r1c0", "Voltage"),
            ],
            BiStrategy::OriginalPlusAppendix,
        );
        let raw_out2 = EpubRawBackend::default()
            .parse(&out2)
            .unwrap();
        // 附录落在最后 spine 文档（ch3，表格所在部件）；表格原样、格译文
        // 不入正文也不入附录（附录只收段落类）
        let raw_in = EpubRawBackend::default()
            .parse(&in_path)
            .unwrap();
        let ch3_out = &raw_out2.pages[2].items[0].text;
        assert!(ch3_out.contains("<p>See the table.</p>"), "段落译文进附录");
        assert!(!ch3_out.contains("Voltage"), "格译文不进附录：{ch3_out}");
        assert!(ch3_out.contains("<td>电压</td>"), "表格原样");
        let body_end = ch3_out.rfind("</body>").unwrap();
        let h2 = ch3_out.find("<h2>译文</h2>").unwrap();
        assert!(h2 < body_end, "附录在 </body> 之前");
        // 其余 spine 文档不动
        assert_eq!(raw_in.pages[0].items[0].text, raw_out2.pages[0].items[0].text);
        assert_eq!(raw_in.pages[1].items[0].text, raw_out2.pages[1].items[0].text);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------------------
    // 部件级交替（writeback_epub_alternating）
    // -------------------------------------------------------------------------

    /// 交替结构金测：spine 序 = 原1/译1/原2/译2，manifest 条目齐且 href 与
    /// 包内名 resolve 自洽，全部既有条目字节与输入完全一致（原部件不动），
    /// mimetype 仍居首且 Stored。
    #[test]
    fn alternating_spine_order_and_untouched_originals() {
        let dir = case_dir("alt-structure");
        let in_path = build_epub_parts(&dir, &[CH1, CH2]);
        let out_path = dir.join("out.epub");

        // 与 writeback_epub 完全同一单元流（无新锚点体系）
        assert_eq!(
            unit_ids(&in_path),
            ["p第一章-b0", "p甲节-b0", "p乙节-b0", "p乙节-b1"]
        );

        alternating_write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
                ("p乙节-b1", "Point two."),
            ],
        );

        // spine 序 = 原1/译1/原2/译2（itemref 原位插入）
        let opf = zip_entry_string(&out_path, "OEBPS/content.opf");
        assert_eq!(spine_idrefs(&opf), ["ch1", "ch1-tr", "ch2", "ch2-tr"]);
        // manifest 条目齐：同目录 href；包内名 = resolve(opf_dir, href)
        assert!(
            opf.contains(
                r#"<item id="ch1-tr" href="text/ch1.tr.xhtml" media-type="application/xhtml+xml"/>"#
            ),
            "{opf}"
        );
        assert!(opf.contains(
            r#"<item id="ch2-tr" href="text/ch2.tr.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        for (href, name) in [
            ("text/ch1.tr.xhtml", "OEBPS/text/ch1.tr.xhtml"),
            ("text/ch2.tr.xhtml", "OEBPS/text/ch2.tr.xhtml"),
        ] {
            assert!(zip_has_entry(&out_path, name), "{name} 存在");
            assert_eq!(
                crate::backends::epub::resolve("OEBPS", href),
                name,
                "manifest href resolve 回包内名"
            );
        }

        // 全部既有条目字节透传——唯二例外是设计内的编辑点：OPF 替换 +
        // 新增译文部件（原 spine 部件 / nav / container / mimetype 均不动）
        let mut zin = open_zip(&in_path).unwrap();
        let mut zout = open_zip(&out_path).unwrap();
        assert_eq!(zout.len(), zin.len() + 2, "只增两个译文部件");
        for i in 0..zin.len() {
            let mut f = zin.by_index(i).unwrap();
            let name = f.name().unwrap().to_string();
            if name == "OEBPS/content.opf" {
                continue; // 设计内编辑：manifest/spine 插条目（上面已断言内容）
            }
            let mut a = Vec::new();
            f.read_to_end(&mut a).unwrap();
            let mut b = Vec::new();
            zout.by_name(&name).unwrap().read_to_end(&mut b).unwrap();
            assert_eq!(a, b, "{name} 字节透传");
        }
        // mimetype 仍为首条目且 Stored
        assert_eq!(zout.by_index(0).unwrap().name().unwrap(), "mimetype");
        assert_eq!(
            zout.by_index(0).unwrap().compression(),
            zip::CompressionMethod::Stored
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 译文部件内容：命中单元已替换（坍缩页首块换译、切节页逐块换译）、
    /// 未命中单元原样、切节页表格格单元逐格替换；译文部件仍是完整合法
    /// XHTML（结构元素保留，重解析页数翻倍）。
    #[test]
    fn alternating_translation_parts_content() {
        let dir = case_dir("alt-content");
        let in_path = build_epub_parts(&dir, &[CH1, CH2, CH3]);
        let out_path = dir.join("out.epub");
        alternating_write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
                // p乙节-b1 不喂：未命中单元原样保留
                ("p参数节-b0", "See the table."),
                ("p参数节-b1-r0c0", "Name"),
                ("p参数节-b1-r0c1", "Unit"),
                ("p参数节-b1-r1c0", "Voltage"),
                ("p参数节-b1-r1c1", "V"),
                ("p参数节-b1-r2c0", "Spanning note"),
                ("p结论节-b0", "As shown above."),
            ],
        );

        // ch1 译文部件（坍缩页）：首块换译、其余可翻译块清空；表格是结构块
        //（坍缩页无格单元）原样——部件仍是完整合法 XHTML
        let tr1 = zip_entry_string(&out_path, "OEBPS/text/ch1.tr.xhtml");
        assert!(tr1.contains("<h1>Chapter 1</h1>"), "{tr1}");
        assert!(!tr1.contains("第一章"));
        assert!(!tr1.contains("正文一。"), "非首块文本清空");
        assert!(tr1.contains("<td>格甲</td>"), "表格结构块原样");
        assert!(tr1.contains("</body>"), "完整合法 XHTML");

        // ch2 译文部件（切节页）：逐块替换；未命中的 要点二。 原样
        let tr2 = zip_entry_string(&out_path, "OEBPS/text/ch2.tr.xhtml");
        assert!(tr2.contains("<p>Section A body.</p>"), "{tr2}");
        assert!(tr2.contains("<li>Point one.</li>"));
        assert!(tr2.contains("<li>要点二。</li>"), "未命中单元原样");

        // ch3 译文部件（切节页 + 表格）：格单元逐格替换（colspan 行单格）
        let tr3 = zip_entry_string(&out_path, "OEBPS/text/ch3.tr.xhtml");
        assert!(tr3.contains("<p>See the table.</p>"), "{tr3}");
        assert!(tr3.contains("<th>Name</th>"));
        assert!(tr3.contains("<th>Unit</th>"));
        assert!(tr3.contains("<td>Voltage</td>"));
        assert!(tr3.contains("<td>V</td>"));
        assert!(tr3.contains(r#"<td colspan="2">Spanning note</td>"#));
        assert!(!tr3.contains("电压"), "全格已替换");
        assert!(tr3.contains("<h2>参数节</h2>"), "切节标题不是单元，原样保留");

        // 重解析：spine 部件翻倍（6 = 原3 + 译3）
        let raw = EpubRawBackend::default().parse(&out_path).unwrap();
        assert_eq!(raw.pages.len(), 6);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CSS 同目录收益：译文部件 head 的 stylesheet 相对 href 与原部件相同
    ///（head 原样透传，无相对路径重算），且指向包内存在的条目。
    #[test]
    fn alternating_translation_parts_keep_stylesheet() {
        let dir = case_dir("alt-css");
        let in_path =
            build_epub_parts_in(&dir, &[CH1_CSS, CH2_CSS], CSS_ITEM, &[("OEBPS/style.css", STYLE_CSS)]);
        let out_path = dir.join("out.epub");
        // head 在 SKIP_TAGS：单元 id 与无 head 版本完全同口径
        assert_eq!(
            unit_ids(&in_path),
            ["p第一章-b0", "p甲节-b0", "p乙节-b0", "p乙节-b1"]
        );
        alternating_write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
                ("p乙节-b1", "Point two."),
            ],
        );

        let link = r#"href="../style.css""#;
        for name in [
            "OEBPS/text/ch1.xhtml",
            "OEBPS/text/ch1.tr.xhtml",
            "OEBPS/text/ch2.xhtml",
            "OEBPS/text/ch2.tr.xhtml",
        ] {
            let s = zip_entry_string(&out_path, name);
            assert!(s.contains(link), "{name} 样式表 href 与原部件相同：{s}");
        }
        assert!(zip_has_entry(&out_path, "OEBPS/style.css"), "href 指向存在的条目");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无命中不插译文部件（防「相同页翻两遍」）：空译文 map → 输出 = 原包
    /// 透传（OPF 不膨胀、条目数不变、Document 逐块一致）；部分命中 → 只有
    /// 命中的部件插译文部件。
    #[test]
    fn alternating_no_hit_no_translation_parts() {
        let dir = case_dir("alt-empty");
        let in_path = build_epub_parts(&dir, &[CH1, CH2]);
        let out_path = dir.join("out.epub");

        // 空译文：全书无命中
        alternating_write(&in_path, &out_path, &[]);
        let opf = zip_entry_string(&out_path, "OEBPS/content.opf");
        assert!(!opf.contains("-tr"), "OPF 不膨胀：{opf}");
        assert_eq!(spine_idrefs(&opf), ["ch1", "ch2"]);
        let zin = open_zip(&in_path).unwrap();
        let zout = open_zip(&out_path).unwrap();
        assert_eq!(zout.len(), zin.len(), "无新增条目");
        assert_eq!(parse_doc(&out_path), parse_doc(&in_path), "输出 = 原包");

        // 部分命中：只有 ch1 有译文部件
        let dir2 = case_dir("alt-partial");
        let in2 = build_epub_parts(&dir2, &[CH1, CH2]);
        let out2 = dir2.join("out.epub");
        alternating_write(&in2, &out2, &[("p第一章-b0", "Chapter 1")]);
        let opf2 = zip_entry_string(&out2, "OEBPS/content.opf");
        assert_eq!(spine_idrefs(&opf2), ["ch1", "ch1-tr", "ch2"]);
        assert!(opf2.contains(r#"<item id="ch1-tr" href="text/ch1.tr.xhtml""#));
        assert!(!opf2.contains("ch2-tr"), "未命中部件不插译文部件：{opf2}");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// id 撞名防御：manifest 已有 id="ch1-tr"（手工件/重复处理形态）→
    /// 译文 id 顺延 -tr2，原条目保留、spine 顺序不受扰。
    #[test]
    fn alternating_manifest_id_collision_defense() {
        let dir = case_dir("alt-idclash");
        let in_path = build_epub_parts_in(
            &dir,
            &[CH1, CH2],
            r#"<item id="ch1-tr" href="extra.xhtml" media-type="application/xhtml+xml"/>"#,
            &[],
        );
        let out_path = dir.join("out.epub");
        alternating_write(
            &in_path,
            &out_path,
            &[
                ("p第一章-b0", "Chapter 1"),
                ("p甲节-b0", "Section A body."),
                ("p乙节-b0", "Point one."),
                ("p乙节-b1", "Point two."),
            ],
        );
        let opf = zip_entry_string(&out_path, "OEBPS/content.opf");
        assert!(
            opf.contains(r#"<item id="ch1-tr2" href="text/ch1.tr.xhtml""#),
            "撞名顺延 -tr2：{opf}"
        );
        assert!(opf.contains(r#"<item id="ch2-tr" href="text/ch2.tr.xhtml""#));
        assert!(
            opf.contains(r#"<item id="ch1-tr" href="extra.xhtml""#),
            "原 ch1-tr 条目保留"
        );
        assert_eq!(spine_idrefs(&opf), ["ch1", "ch1-tr2", "ch2", "ch2-tr"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OPF 非 UTF-8（UTF-16LE BOM 件）：alternating 整体拒绝——OPF 是译文
    /// 部件的挂载骨架，宁响勿哑（writeback_epub 不动 OPF，无此约束）。
    #[test]
    fn alternating_rejects_non_utf8_opf() {
        let dir = case_dir("alt-opf-enc");
        let in_path = build_epub_parts(&dir, &[CH1, CH2]);
        let opf = zip_entry_string(&in_path, "OEBPS/content.opf");
        let mut utf16: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM（解码链 BOM 段命中）
        for u in opf.encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        let bad = dir.join("bad.epub");
        replace_zip_entry(&in_path, &bad, "OEBPS/content.opf", &utf16);
        let out = dir.join("out.epub");
        let pairs: Vec<crate::emit::TranslationPair> =
            vec![("p第一章-b0".to_string(), "Chapter 1".to_string()).into()];
        let err = writeback_epub_alternating(&bad, &pairs, &out).unwrap_err();
        assert!(err.to_string().contains("OPF 非 UTF-8"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------------------
    // 译文对（TranslationPair）源校验 + 报告
    // -------------------------------------------------------------------------

    /// 源校验一致路径：携带①侧源文本（= to_translation_units 文本）的译文
    /// 对，全部包内条目与不带源（迁移前 `(id, 译文)` 形态）逐字节等价；
    /// written 计数齐全（含格），无跳过。
    #[test]
    fn source_validated_pairs_produce_identical_output() {
        let dir = case_dir("src-ok");
        let in_path = build_epub_parts(&dir, &[CH1, CH2, CH3]);
        let units = to_translation_units(&parse_doc(&in_path));

        let a = dir.join("a.epub");
        let b = dir.join("b.epub");
        // 不带源：`(id, 译文)` 二元组 From（= 迁移前形态）
        let plain: Vec<crate::emit::TranslationPair> = units
            .iter()
            .map(|u| (u.id.clone(), format!("译·{}", u.text)).into())
            .collect();
        let rep_plain = writeback_epub(&in_path, &plain, BiStrategy::TranslationOnly, &a).unwrap();

        // 带一致源文本：`(id, 源, 译文)` 三元组 From
        let checked: Vec<crate::emit::TranslationPair> = units
            .iter()
            .map(|u| (u.id.clone(), u.text.clone(), format!("译·{}", u.text)).into())
            .collect();
        let rep_checked =
            writeback_epub(&in_path, &checked, BiStrategy::TranslationOnly, &b).unwrap();

        // 全部条目逐字节一致（改写件 + 透传件都覆盖，条目序同）
        let mut za = open_zip(&a).unwrap();
        let mut zb = open_zip(&b).unwrap();
        assert_eq!(za.len(), zb.len());
        for i in 0..za.len() {
            let name = za.by_index(i).unwrap().name().unwrap().to_string();
            let mut ba = Vec::new();
            za.by_name(&name).unwrap().read_to_end(&mut ba).unwrap();
            let mut bb = Vec::new();
            zb.by_name(&name).unwrap().read_to_end(&mut bb).unwrap();
            assert_eq!(ba, bb, "{name} 字节一致");
        }
        assert_eq!(rep_checked.written, units.len(), "全部单元写入（含格）");
        assert!(rep_checked.skipped.is_empty(), "一致源不产生跳过");
        assert_eq!(rep_plain.written, rep_checked.written);
        assert_eq!(rep_plain.skipped, rep_checked.skipped);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 源校验失败路径：段落与表格格各一例——该单元跳过（报告记 id 与
    /// 原因）、该处原文保留；一致单元照常写入（部分命中计数）。
    #[test]
    fn source_mismatch_skips_para_and_cell_keeps_original() {
        let dir = case_dir("src-bad");
        let in_path = build_epub_parts(&dir, &[CH3]);
        let out_path = dir.join("out.epub");
        let units = to_translation_units(&parse_doc(&in_path));
        // 坏源目标按**文本**锁定（段落「如下表所示。」+ 格「电压」）——
        // 块序/单元数随 fixture 演进，id 动态推导而非硬编码
        let bad_para_id = units
            .iter()
            .find(|u| u.text == "如下表所示。")
            .expect("fixture 含该段落单元")
            .id
            .clone();
        let bad_cell_id = units
            .iter()
            .find(|u| u.text == "电压")
            .expect("fixture 含该格单元")
            .id
            .clone();
        assert!(units.len() >= 3, "至少：坏段 + 坏格 + 一致格");

        let bad = [bad_para_id.as_str(), bad_cell_id.as_str()];
        let pairs: Vec<crate::emit::TranslationPair> = units
            .iter()
            .map(|u| {
                let src = if bad.contains(&u.id.as_str()) {
                    format!("（提取后文件已变更）{}", u.text)
                } else {
                    u.text.clone()
                };
                (u.id.clone(), src, format!("译·{}", u.text)).into()
            })
            .collect();
        let report = writeback_epub(&in_path, &pairs, BiStrategy::TranslationOnly, &out_path).unwrap();

        assert_eq!(report.written, units.len() - 2, "其余单元全部写入（含格）");
        assert_eq!(
            report.skipped,
            [
                (
                    bad_para_id.clone(),
                    "源文本不一致（提取后文件已变更）".to_string()
                ),
                (
                    bad_cell_id.clone(),
                    "源文本不一致（提取后文件已变更）".to_string()
                ),
            ],
            "跳过按文档序记 id 与原因"
        );

        // 跳过单元该处原文保留；一致单元照常替换
        let raw = EpubRawBackend::default().parse(&out_path).unwrap();
        let ch3 = &raw.pages[0].items[0].text;
        assert!(ch3.contains("<p>如下表所示。</p>"), "坏源段落原样：{ch3}");
        assert!(ch3.contains("<td>电压</td>"), "坏源格原样：{ch3}");
        assert!(ch3.contains("<th>译·名称</th>"), "一致格照常写入：{ch3}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
