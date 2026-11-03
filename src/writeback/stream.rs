// docforge 新增（翻译闭环②写侧）：流式回写会话 WritebackSession。
//! 流式回写会话：译文按翻译单元流式喂入（[`WritebackSession::push`]）、
//! spill 临时文件落盘不驻留 RAM、[`WritebackSession::finish`] 回放并分发到
//! 对应回写器——补齐「解析→翻→写」流式闭环的最后一环。
//!
//! # 管线形态（与全量回写 API 的分工）
//!
//! 解析侧 [`crate::engine::DocumentEngine::parse_streaming`]（超大 pdf 用
//! 深度流式入口 `crate::backends::pdf::parse_streaming`）逐页投递，页内组
//! 翻译单元、译文产一条喂一条，全部完成后一次性打包：
//!
//! ```no_run
//! use docforge::emit::{to_translation_units, BiStrategy};
//! use docforge::model::{Document, Page};
//! use docforge::writeback::stream::{WritebackFormat, WritebackSession};
//!
//! fn translate(text: &str) -> String { text.to_string() } // ← 翻译引擎接在这里
//!
//! # fn main() -> Result<(), docforge::DocError> {
//! let mut session = WritebackSession::new(
//!     WritebackFormat::Txt, "in.txt", "out-bilingual.txt", BiStrategy::default())?;
//! docforge::parse_streaming("in.txt", &mut |page: Page| {
//!     // 逐页组单元：id = `p{页名}-b{块序}`，多页后端（pdf/epub/fb2/字幕）
//!     // 都有页名，逐页包装与全文档组单元同 id 同序；无名页的自定义后端
//!     // 需消费方自行维护全局页序再组单元。
//!     let doc = Document { pages: vec![page], ..Default::default() };
//!     for u in to_translation_units(&doc) {
//!         let tr = translate(&u.text);
//!         if session.push(&u.id, &tr).is_err() {
//!             return false; // spill 写失败：中止投递（meta warnings 记中止页）
//!         }
//!     }
//!     true
//! })?;
//! session.finish()?; // 回放 spill → 分发到 writeback_txt 打包
//! # Ok(())
//! # }
//! ```
//!
//! 相对「全量 API（`writeback_*` 吃 `&[(String, String)]`）+ 调用方自持译文
//! 全集」，本会话少一份生产侧驻留，且 push / finish 分离让译文生产（翻译
//! 引擎逐条出）与写盘（finish 打包）解耦。
//!
//! # 内存语义（诚实边界）
//!
//! * push 阶段译文**零驻留**：每条译文以定长帧追加进 spill 临时文件后即弃
//!   （每帧直写无缓冲，`push` 返回时字节已在盘上）；
//! * finish 阶段**单份译文全集驻留**：回放组装 `Vec<(String, String)>` 交给
//!   回写器。这是容器格式的现实——docx / epub / odt 是 zip 重打包、xml 单
//!   件改写器也是一次性吃全集的既有 API，真·逐条增量落盘只对 txt / md
//!   可行，v1 不做；API 形状已留出（`finish` 之外的增量出口未来加
//!   `finish_incremental` 不破坏调用方）。
//!
//! # spill 帧格式与防护
//!
//! 临时文件 `temp_dir()/docforge-wb-{pid}-{unix纳秒}.spill`；帧 =
//! `(u32 LE id 字节数, id UTF-8 字节, u32 LE 译文字节数, 译文 UTF-8 字节)`。
//! 单帧（id / 译文各计）上限 [`MAX_FRAME_BYTES`]：写侧超限当场拒绝，读侧
//! 超限 / 截断 / 非 UTF-8 报 `corrupt`——防损坏文件的长度头炸内存。`Drop`
//! 兜底删除 spill；`finish` 成败路径都删（Drop 对已删文件静默）。
//!
//! # 契约
//!
//! * 空白译文（trim 后为空）静默忽略——与 [`super::translation_map`] 的
//!   「空白视为未提供」同口径；`push` 返回**已接受总条数**（含重复 id）供
//!   管线统计；
//! * 重复 id：回放保持投递序，交由回写器侧 `translation_map` 的 HashMap
//!   语义**后者胜**（与全量 API 一致）；未匹配 id 由回写器忽略（既有语义，
//!   会话层不校验 id——不解析原文，id 只是不透明串）;
//! * `new` 可行性前置检查：格式对应的回写 feature 未编译 → 当场
//!   [`crate::error::ErrorCode::Unsupported`]（不是跑到 finish 才炸）。
//!   枚举变体**不受 cfg 裁剪**（调用方代码在 feature 开关组合间可稳定
//!   命名所有格式），可用性由 `new` 把关。
//!
//! # 覆盖格式与门控
//!
//! [`WritebackFormat`] 七种：Txt 恒开（零依赖）；Docx/Epub/Html/Fb2/Odt
//! 各随同名 feature（finish 分发到对应 `writeback_*`，见 writeback 模块
//! 级文档）；Markdown 恒在但解析侧随 feature `md`（默认开，回写 =
//! 重解析 + [`crate::emit::to_bilingual_markdown_with`] 纯文本重组写文件）。

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(feature = "docx")]
use super::writeback_docx;
#[cfg(feature = "epub")]
use super::writeback_epub;
#[cfg(feature = "fb2")]
use super::writeback_fb2;
#[cfg(feature = "html")]
use super::writeback_html;
#[cfg(feature = "odt")]
use super::writeback_odt;
use super::writeback_txt;
#[cfg(feature = "md")]
use crate::plugin::{FormatPlugin as _, ParseCapability as _};
#[cfg(feature = "md")]
use crate::plugins::markdown::MarkdownPlugin;
#[cfg(feature = "md")]
use crate::plugins::text::WholeTextParseCapability;
use crate::emit::BiStrategy;
use crate::error::{DocError, Result};

/// 单帧长度上限（id / 译文各计 16 MiB）：防损坏 spill 的长度头炸内存。
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// spill 文件名前缀（temp_dir 扫描与测试探针共用口径）。
const SPILL_PREFIX: &str = "docforge-wb-";

/// 回写目标格式。变体恒在（不受 cfg 裁剪，见模块级文档「契约」）；可用性
/// 由 [`WritebackSession::new`] 前置检查把关：对应 feature 未编译 →
/// `Unsupported`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritebackFormat {
    /// DOCX（feature `docx`）。
    Docx,
    /// EPUB（feature `epub`）。
    Epub,
    /// HTML 单文件（feature `html`）。
    Html,
    /// FB2 单 XML（feature `fb2`；fb2.zip 容器不支持，同 `writeback_fb2`）。
    Fb2,
    /// ODT（feature `odt`）。
    Odt,
    /// TXT（恒开，零依赖）。
    Txt,
    /// Markdown（恒在；重解析随 feature `md`，默认开）。
    Markdown,
}

/// 流式回写会话：`new` 建会话（spill 就绪）→ 逐条 [`Self::push`] →
/// [`Self::finish`] 打包。消费 `in_path` 原文件、产出 `out_path`，语义与
/// 对应 `writeback_*` 全量 API 完全一致（同一解析器、同一策略、同一忽略
/// 口径），只是译文的喂入方式从「一次性全集」变成「流式 + spill」。
#[derive(Debug)]
pub struct WritebackSession {
    fmt: WritebackFormat,
    in_path: PathBuf,
    out_path: PathBuf,
    strategy: BiStrategy,
    spill_path: PathBuf,
    /// spill 写柄（每帧直写无缓冲；`finish` 关闭后置 None）。
    spill: Option<File>,
    /// 已接受条数（非空白 push 的累计值；`push` 的返回值）。
    accepted: usize,
}

impl WritebackSession {
    /// 建会话：可行性前置检查 + spill 临时文件就绪（建文件失败当场报，
    /// 不留到 finish）。对应 feature 未编译 → [`crate::error::ErrorCode::Unsupported`]。
    pub fn new(
        fmt: WritebackFormat,
        in_path: &str,
        out_path: &str,
        strategy: BiStrategy,
    ) -> Result<Self> {
        // 前置检查：feature 未编译的格式当场拒绝（枚举变体不受 cfg 裁剪，
        // 可用性在此把关——调用方代码在开关组合间稳定，错误信息直接指路 feature）
        match fmt {
            #[cfg(feature = "docx")]
            WritebackFormat::Docx => {}
            #[cfg(not(feature = "docx"))]
            WritebackFormat::Docx => return Err(unsupported("docx")),
            #[cfg(feature = "epub")]
            WritebackFormat::Epub => {}
            #[cfg(not(feature = "epub"))]
            WritebackFormat::Epub => return Err(unsupported("epub")),
            #[cfg(feature = "html")]
            WritebackFormat::Html => {}
            #[cfg(not(feature = "html"))]
            WritebackFormat::Html => return Err(unsupported("html")),
            #[cfg(feature = "fb2")]
            WritebackFormat::Fb2 => {}
            #[cfg(not(feature = "fb2"))]
            WritebackFormat::Fb2 => return Err(unsupported("fb2")),
            #[cfg(feature = "odt")]
            WritebackFormat::Odt => {}
            #[cfg(not(feature = "odt"))]
            WritebackFormat::Odt => return Err(unsupported("odt")),
            WritebackFormat::Txt => {}
            #[cfg(feature = "md")]
            WritebackFormat::Markdown => {}
            #[cfg(not(feature = "md"))]
            WritebackFormat::Markdown => return Err(unsupported("md")),
        }

        // spill 就绪：pid + 纳秒双保险避免同进程多会话 / 快速重建撞名
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let spill_path = std::env::temp_dir().join(format!(
            "{SPILL_PREFIX}{}-{}.spill",
            std::process::id(),
            nanos
        ));
        let spill = File::create(&spill_path)
            .map_err(|e| DocError::corrupt(format!("cannot create {}: {e}", spill_path.display())))?;

        Ok(Self {
            fmt,
            in_path: PathBuf::from(in_path),
            out_path: PathBuf::from(out_path),
            strategy,
            spill_path,
            spill: Some(spill),
            accepted: 0,
        })
    }

    /// 喂入一条译文（id = [`crate::emit::to_translation_units`] 的锚点）。
    /// 空白译文静默忽略（与 `translation_map` 口径一致）；返回已接受总条数。
    /// 译文按帧追加落盘即弃——本调用不驻留译文 RAM。
    pub fn push(&mut self, id: &str, translation: &str) -> Result<usize> {
        if translation.trim().is_empty() {
            return Ok(self.accepted);
        }
        let id_bytes = id.as_bytes();
        let tr_bytes = translation.as_bytes();
        if id_bytes.len() > MAX_FRAME_BYTES || tr_bytes.len() > MAX_FRAME_BYTES {
            return Err(DocError::corrupt(format!(
                "spill 帧超限（id/译文各上限 {MAX_FRAME_BYTES} 字节）"
            )));
        }
        let w = self
            .spill
            .as_mut()
            .ok_or_else(|| DocError::corrupt("spill 写柄已关闭"))?;
        write_frame(w, id_bytes)?;
        write_frame(w, tr_bytes)?;
        self.accepted += 1;
        Ok(self.accepted)
    }

    /// 回放 spill → 组 (id, 译文) 列表（重复 id 后者胜，HashMap 语义）→
    /// 分发到对应回写器。成败路径都删除 spill（`Drop` 兜底二次删除静默）。
    pub fn finish(mut self) -> Result<()> {
        let outcome = self.finish_inner();
        drop(self.spill.take()); // 先关写柄再删（删打开中的文件在部分平台失败）
        let _ = std::fs::remove_file(&self.spill_path);
        outcome
    }

    /// finish 的实际工作：刷净写柄 → 回放 → 按格式分发（各回写器与全量
    /// API 同一入口，语义零分叉）。
    fn finish_inner(&mut self) -> Result<()> {
        if let Some(w) = self.spill.as_mut() {
            w.flush()
                .map_err(|e| DocError::corrupt(format!("spill flush 失败: {e}")))?;
        }
        let pairs = replay(&self.spill_path)?;
        match self.fmt {
            WritebackFormat::Txt => {
                writeback_txt(&self.in_path, &pairs, self.strategy, &self.out_path)
            }
            #[cfg(feature = "md")]
            WritebackFormat::Markdown => self.finish_markdown(&pairs),
            // new 已挡（Unsupported）；防御性分支只兜「编译面」完整性
            #[cfg(not(feature = "md"))]
            WritebackFormat::Markdown => Err(unsupported("md")),
            #[cfg(feature = "docx")]
            WritebackFormat::Docx => {
                writeback_docx(&self.in_path, &pairs, self.strategy, &self.out_path)
            }
            #[cfg(not(feature = "docx"))]
            WritebackFormat::Docx => Err(unsupported("docx")),
            #[cfg(feature = "epub")]
            WritebackFormat::Epub => {
                writeback_epub(&self.in_path, &pairs, self.strategy, &self.out_path)
            }
            #[cfg(not(feature = "epub"))]
            WritebackFormat::Epub => Err(unsupported("epub")),
            #[cfg(feature = "html")]
            WritebackFormat::Html => {
                writeback_html(&self.in_path, &pairs, self.strategy, &self.out_path)
            }
            #[cfg(not(feature = "html"))]
            WritebackFormat::Html => Err(unsupported("html")),
            #[cfg(feature = "fb2")]
            WritebackFormat::Fb2 => {
                writeback_fb2(&self.in_path, &pairs, self.strategy, &self.out_path)
            }
            #[cfg(not(feature = "fb2"))]
            WritebackFormat::Fb2 => Err(unsupported("fb2")),
            #[cfg(feature = "odt")]
            WritebackFormat::Odt => {
                writeback_odt(&self.in_path, &pairs, self.strategy, &self.out_path)
            }
            #[cfg(not(feature = "odt"))]
            WritebackFormat::Odt => Err(unsupported("odt")),
        }
    }

    /// Markdown 回写：提取侧同管线（`WholeTextParseCapability` + `MarkdownPlugin`，
    /// engine 装配 md 的同一组合）重解析 → [`crate::emit::to_bilingual_markdown_with`]
    /// 纯文本重组 → 写文件。
    #[cfg(feature = "md")]
    fn finish_markdown(&self, pairs: &[(String, String)]) -> Result<()> {
        let path_str = self
            .in_path
            .to_str()
            .ok_or_else(|| DocError::corrupt("md 路径不是 UTF-8"))?;
        let raw = WholeTextParseCapability { id: "text.read.md", fmt: "md" }.parse(path_str)?;
        let doc = MarkdownPlugin.assemble(&raw);
        let md = crate::emit::to_bilingual_markdown_with(&doc, pairs, self.strategy);
        std::fs::write(&self.out_path, md)
            .map_err(|e| DocError::corrupt(format!("cannot create {}: {e}", self.out_path.display())))
    }
}

impl Drop for WritebackSession {
    fn drop(&mut self) {
        // 兜底清理：finish 已删的文件静默；先关写柄再删
        self.spill = None;
        let _ = std::fs::remove_file(&self.spill_path);
    }
}

/// feature 未编译的统一报错（文案与 engine 的 no_plugin_message 同款指路）。
/// 全部 feature 皆开时调用点（`cfg(not)` 分支）归零——允许 dead，与
/// engine.rs 的 `_code` 同款处理。
#[allow(dead_code)]
fn unsupported(feature: &str) -> DocError {
    DocError::unsupported(format!(
        "回写格式未编译：在 Cargo.toml 的 docforge 依赖加 features = [\"{feature}\"] 后重新编译"
    ))
}

/// 写一帧：`u32 LE 长度 + UTF-8 字节`。调用方已做长度上限检查。
fn write_frame(w: &mut File, bytes: &[u8]) -> Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|_| w.write_all(bytes))
        .map_err(|e| DocError::corrupt(format!("spill 写入失败: {e}")))
}

/// 读一帧；EOF 恰在帧头 → None（干净收尾），帧体截断 / 长度超限 / 非
/// UTF-8 → `corrupt`（防损坏文件的长度头炸内存）。
fn read_frame(r: &mut File) -> Result<Option<String>> {
    let mut head = [0u8; 4];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(DocError::corrupt(format!("spill 读取失败: {e}"))),
    }
    let len = u32::from_le_bytes(head) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(DocError::corrupt(format!(
            "spill 帧长度超限（{len} > {MAX_FRAME_BYTES}，文件可能损坏）"
        )));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)
        .map_err(|e| DocError::corrupt(format!("spill 帧截断: {e}")))?;
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| DocError::corrupt("spill 帧非 UTF-8（文件可能损坏）"))
}

/// 回放 spill：逐帧读回 (id, 译文) 对，保持投递序（重复 id 后者胜——交由
/// 回写器侧 translation_map 的 HashMap 语义收敛）。
fn replay(spill_path: &Path) -> Result<Vec<(String, String)>> {
    let mut r = File::open(spill_path)
        .map_err(|e| DocError::corrupt(format!("cannot open {}: {e}", spill_path.display())))?;
    let mut pairs = Vec::new();
    while let Some(id) = read_frame(&mut r)? {
        let tr = read_frame(&mut r)?
            .ok_or_else(|| DocError::corrupt("spill 帧截断：id 有译文无"))?;
        pairs.push((id, tr));
    }
    Ok(pairs)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::emit::to_translation_units;

    fn case_dir(case: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "docforge_writeback_stream_{}_{}",
            std::process::id(),
            case
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pairs_of(tr: &[(&str, &str)]) -> Vec<(String, String)> {
        tr.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    /// temp 目录里 `docforge-wb-{pid}-*.spill` 前缀的现存文件。
    fn spill_files() -> Vec<PathBuf> {
        let prefix = format!("{SPILL_PREFIX}{}-", std::process::id());
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            for e in entries.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix) && name.ends_with(".spill") {
                    out.push(e.path());
                }
            }
        }
        out.sort();
        out
    }

    /// 当前 spill 文件里含 `marker` 内容的个数（并行测试同进程共享 pid，
    /// 按内容认领本测试自己的 spill，避免他测会话的存活 spill 干扰计数）。
    fn spills_containing(marker: &str) -> usize {
        spill_files()
            .iter()
            .filter(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains(marker)))
            .count()
    }

    /// txt 全流程（流式管线形态）：parse → units → 逐条 push → finish，
    /// 输出字节 == 直接调 `writeback_txt` 的输出（证明分发无失真）；顺带
    /// 覆盖重复 id 后者胜与未匹配 id 忽略。
    #[test]
    fn txt_stream_matches_direct_writeback() {
        let dir = case_dir("txt");
        let in_path = dir.join("in.txt");
        std::fs::write(
            &in_path,
            "第一章 起\n\n清晨他离开村庄。\n\n第二章 归来\n\n多年以后他回来了。\n",
        )
        .unwrap();

        let doc = crate::parse(in_path.to_str().unwrap()).unwrap();
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 4);

        // 流式路径：全单元逐条 push（含一条重复 id 的「修订译文」压轴）
        let out_stream = dir.join("out-stream.txt");
        let mut session = WritebackSession::new(
            WritebackFormat::Txt,
            in_path.to_str().unwrap(),
            out_stream.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        let last_id = units[units.len() - 1].id.clone();
        let mut accepted = 0usize;
        for u in &units {
            accepted = session.push(&u.id, &format!("EN: {}", u.text)).unwrap();
        }
        assert_eq!(accepted, units.len(), "push 返回累计接受条数");
        accepted = session.push(&last_id, "EN(revised): The final line.").unwrap();
        assert_eq!(accepted, units.len() + 1);
        session.finish().unwrap();

        // 直接路径：同一份 (id, 译文) 全集（含重复 id 与未匹配 id）
        let mut direct: Vec<(String, String)> = units
            .iter()
            .map(|u| (u.id.clone(), format!("EN: {}", u.text)))
            .collect();
        direct.push((last_id.clone(), "EN(revised): The final line.".into()));
        direct.push(("p0-b999".into(), "bogus".into()));
        let out_direct = dir.join("out-direct.txt");
        writeback_txt(&in_path, &direct, BiStrategy::BilingualInline, &out_direct).unwrap();

        assert_eq!(
            std::fs::read(&out_stream).unwrap(),
            std::fs::read(&out_direct).unwrap(),
            "流式分发与全量 API 输出逐字节一致"
        );
        // 重复 id 后者胜：压轴修订译文出现在最终文件里
        let out = std::fs::read_to_string(&out_stream).unwrap();
        assert!(out.contains("EN(revised): The final line."));
        assert!(!out.contains("bogus"), "未匹配 id 忽略");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Markdown：finish 后文件 == `to_bilingual_markdown_with` 结果写出。
    #[test]
    #[cfg(feature = "md")]
    fn markdown_stream_matches_direct_emit() {
        let dir = case_dir("md");
        let in_path = dir.join("in.md");
        std::fs::write(
            &in_path,
            "# 标题\n\n正文段落甲。\n\n- 列表项一。\n- 列表项二。\n\n尾段一句。\n",
        )
        .unwrap();

        let doc = crate::parse(in_path.to_str().unwrap()).unwrap();
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 4, "标题 / 段落 / 合并列表 / 尾段");

        let out_path = dir.join("out.md");
        let mut session = WritebackSession::new(
            WritebackFormat::Markdown,
            in_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        for u in &units {
            session.push(&u.id, &format!("EN: {}", u.text)).unwrap();
        }
        session.finish().unwrap();

        let direct: Vec<(String, String)> = units
            .iter()
            .map(|u| (u.id.clone(), format!("EN: {}", u.text)))
            .collect();
        let want = crate::emit::to_bilingual_markdown_with(&doc, &direct, BiStrategy::BilingualInline);
        assert_eq!(
            std::fs::read_to_string(&out_path).unwrap(),
            want,
            "Markdown 分发与直接 emit 结果一致"
        );
        assert!(want.contains("EN: 正文段落甲。"), "双语对照进了文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// docx（cfg docx）：最小 docx 往返 —— push 段落译文 → finish → 重解析
    /// 输出含译文（原/译交错）。
    #[test]
    #[cfg(feature = "docx")]
    fn docx_roundtrip_reparse_contains_translations() {
        use crate::backends::office::docx::DocxRawBackend;
        use crate::backends::office::OfficePlugin;
        use crate::plugin::{FormatPlugin as _, ParseCapability as _};
        use std::io::Write as _;

        let dir = case_dir("docx");
        let in_path = dir.join("in.docx");
        let document_xml = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>第一章</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>这是第一段正文。</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>第二段。</w:t></w:r></w:p>"#,
            r#"</w:body></w:document>"#
        );
        let f = std::fs::File::create(&in_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in [
            ("word/document.xml", document_xml),
            ("word/styles.xml", "<w:styles/>"),
            ("[Content_Types].xml", "<Types/>"),
        ] {
            zw.start_file(name, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        }
        zw.finish().unwrap();

        // 提取侧同管线（DocxRawBackend + OfficePlugin）组单元
        let raw = DocxRawBackend::default().parse(in_path.to_str().unwrap()).unwrap();
        let doc = OfficePlugin::default().assemble(&raw);
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 3);

        let out_path = dir.join("out.docx");
        let mut session = WritebackSession::new(
            WritebackFormat::Docx,
            in_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        for u in &units {
            session.push(&u.id, &format!("EN: {}", u.text)).unwrap();
        }
        session.finish().unwrap();

        // 重解析输出：原/译交错（标题复制 pStyle 仍是标题）
        let raw = DocxRawBackend::default().parse(out_path.to_str().unwrap()).unwrap();
        let got = OfficePlugin::default().assemble(&raw);
        let contents: Vec<&str> = got.pages[0].blocks.iter().map(|b| b.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "第一章",
                "EN: 第一章",
                "这是第一段正文。",
                "EN: 这是第一段正文。",
                "第二段。",
                "EN: 第二段。",
            ]
        );
        assert_eq!(got.pages[0].blocks[1].kind, crate::model::BlockKind::Heading);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// html（cfg html）：push → finish 不炸 + 输出可重解析且含译文。
    #[test]
    #[cfg(feature = "html")]
    fn html_smoke_roundtrip() {
        use crate::plugins::html::HtmlPlugin;
        use crate::plugins::text::WholeTextParseCapability;
        use crate::plugin::{FormatPlugin as _, ParseCapability as _};

        let dir = case_dir("html");
        let in_path = dir.join("in.html");
        std::fs::write(
            &in_path,
            "<html><body><h1>标题</h1><p>段落甲。</p></body></html>",
        )
        .unwrap();

        let raw = WholeTextParseCapability { id: "html.parse", fmt: "html" }
            .parse(in_path.to_str().unwrap())
            .unwrap();
        let doc = HtmlPlugin.assemble(&raw);
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 2);

        let out_path = dir.join("out.html");
        let mut session = WritebackSession::new(
            WritebackFormat::Html,
            in_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        for u in &units {
            session.push(&u.id, &format!("EN: {}", u.text)).unwrap();
        }
        session.finish().unwrap();

        let raw = WholeTextParseCapability { id: "html.parse", fmt: "html" }
            .parse(out_path.to_str().unwrap())
            .unwrap();
        let got = HtmlPlugin.assemble(&raw);
        let contents: Vec<&str> = got.pages[0].blocks.iter().map(|b| b.content.as_str()).collect();
        assert!(contents.contains(&"EN: 标题"), "实际块序：{contents:?}");
        assert!(contents.contains(&"EN: 段落甲。"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// epub（cfg epub）：push → finish 不炸 + 输出可重解析且含译文。
    #[test]
    #[cfg(feature = "epub")]
    fn epub_smoke_roundtrip() {
        use crate::backends::epub::{EpubPlugin, EpubRawBackend};
        use crate::plugin::{FormatPlugin as _, ParseCapability as _};
        use std::io::Write as _;

        let dir = case_dir("epub");
        let in_path = dir.join("in.epub");
        let f = std::fs::File::create(&in_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        let mut put = |name: &str, body: &str| {
            zw.start_file(name, opts).unwrap();
            zw.write_all(body.as_bytes()).unwrap();
        };
        put("mimetype", "application/epub+zip");
        put(
            "META-INF/container.xml",
            r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        );
        put(
            "OEBPS/content.opf",
            r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>测试书</dc:title><dc:language>zh</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#,
        );
        put(
            "OEBPS/nav.xhtml",
            r#"<?xml version="1.0"?>
<html xmlns="http://www.w3.org/1999/xhtml"><body>
<nav epub:type="toc" xmlns:epub="http://www.idpf.org/2007/ops">
  <ol><li><a href="text/ch1.xhtml">第一章</a></li></ol>
</nav></body></html>"#,
        );
        put(
            "OEBPS/text/ch1.xhtml",
            "<html><body><h1>第一章</h1><p>正文一段。</p></body></html>",
        );
        zw.finish().unwrap();

        let raw = EpubRawBackend::default().parse(in_path.to_str().unwrap()).unwrap();
        let doc = EpubPlugin.assemble(&raw);
        let units = to_translation_units(&doc);
        // 坍缩页语义：单 spine 文档坍缩为一页，首个 h1 可能升为页名（不占块位）
        assert!(!units.is_empty(), "坍缩页应有可翻译单元");

        let out_path = dir.join("out.epub");
        let mut session = WritebackSession::new(
            WritebackFormat::Epub,
            in_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        for u in &units {
            session.push(&u.id, &format!("EN: {}", u.text)).unwrap();
        }
        session.finish().unwrap();

        let raw = EpubRawBackend::default().parse(out_path.to_str().unwrap()).unwrap();
        let got = EpubPlugin.assemble(&raw);
        let contents: Vec<&str> = got
            .pages
            .iter()
            .flat_map(|p| p.blocks.iter().map(|b| b.content.as_str()))
            .collect();
        for u in &units {
            let tr = format!("EN: {}", u.text);
            assert!(
                contents.iter().any(|c| c.contains(&tr)),
                "译文落盘：{tr} / 实际块序：{contents:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// odt（cfg odt）：push → finish 不炸 + 输出可重解析且含译文。
    #[test]
    #[cfg(feature = "odt")]
    fn odt_smoke_roundtrip() {
        use crate::backends::odf::OdfPlugin;
        use crate::backends::odf::odt::OdtRawBackend;
        use crate::plugin::{FormatPlugin as _, ParseCapability as _};
        use std::io::Write as _;

        let dir = case_dir("odt");
        let in_path = dir.join("in.odt");
        let content_xml = concat!(
            r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text>"#,
            r#"<text:h text:outline-level="1" text:style-name="Heading_20_1">标题一</text:h>"#,
            r#"<text:p>正文一段。</text:p>"#,
            r#"</office:text></office:body></office:document-content>"#
        );
        let f = std::fs::File::create(&in_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        zw.start_file(
            "mimetype",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zw.write_all(b"application/vnd.oasis.opendocument.text").unwrap();
        zw.start_file("styles.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"<office:document-styles/>").unwrap();
        zw.start_file("content.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(content_xml.as_bytes()).unwrap();
        zw.finish().unwrap();

        let raw = OdtRawBackend.parse(in_path.to_str().unwrap()).unwrap();
        let doc = OdfPlugin.assemble(&raw);
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 2);

        let out_path = dir.join("out.odt");
        let mut session = WritebackSession::new(
            WritebackFormat::Odt,
            in_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        for u in &units {
            session.push(&u.id, &format!("EN: {}", u.text)).unwrap();
        }
        session.finish().unwrap();

        let raw = OdtRawBackend.parse(out_path.to_str().unwrap()).unwrap();
        let got = OdfPlugin.assemble(&raw);
        let contents: Vec<&str> = got.pages[0].blocks.iter().map(|b| b.content.as_str()).collect();
        assert!(contents.contains(&"EN: 标题一"), "实际块序：{contents:?}");
        assert!(contents.contains(&"EN: 正文一段。"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// fb2（cfg fb2）：push → finish 不炸 + 输出可重解析且含译文。
    #[test]
    #[cfg(feature = "fb2")]
    fn fb2_smoke_roundtrip() {
        use crate::backends::fb2::{Fb2Plugin, Fb2RawBackend};
        use crate::plugin::{FormatPlugin as _, ParseCapability as _};

        let dir = case_dir("fb2");
        let in_path = dir.join("in.fb2");
        std::fs::write(
            &in_path,
            r#"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/FictionBook/2.0">
<body>
  <section><title><p>第一章</p></title><p>正文一段。</p></section>
</body>
</FictionBook>"#,
        )
        .unwrap();

        let raw = Fb2RawBackend::default().parse(in_path.to_str().unwrap()).unwrap();
        let doc = Fb2Plugin.assemble(&raw);
        let units = to_translation_units(&doc);
        assert_eq!(units.len(), 2);

        let out_path = dir.join("out.fb2");
        let mut session = WritebackSession::new(
            WritebackFormat::Fb2,
            in_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        for u in &units {
            session.push(&u.id, &format!("EN: {}", u.text)).unwrap();
        }
        session.finish().unwrap();

        let raw = Fb2RawBackend::default().parse(out_path.to_str().unwrap()).unwrap();
        let got = Fb2Plugin.assemble(&raw);
        let contents: Vec<&str> = got.pages.iter().flat_map(|p| p.blocks.iter().map(|b| b.content.as_str())).collect();
        assert!(contents.contains(&"EN: 第一章"), "实际块序：{contents:?}");
        assert!(contents.contains(&"EN: 正文一段。"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// spill 清理：构造 + push 几条 + drop（不 finish）→ temp 目录
    /// `docforge-wb-{pid}-*.spill` 计数归零（finish 路径则不留任何 spill）。
    #[test]
    fn spill_removed_on_drop_and_finish() {
        let dir = case_dir("spill");
        let in_path = dir.join("in.txt");
        std::fs::write(&in_path, "第一章 起\n\n清晨他离开村庄。\n").unwrap();
        // 内容探针：本测试独占的标记串（并行测试共享 pid，按内容认领自己的 spill）
        const MARKER: &str = "spill-cleanup-probe-9781";

        // drop 路径：spill 随 drop 删除
        let out_path = dir.join("out-drop.txt");
        let mut session = WritebackSession::new(
            WritebackFormat::Txt,
            in_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        session.push("p0-b0", MARKER).unwrap();
        assert_eq!(spills_containing(MARKER), 1, "push 后 spill 在盘上");
        drop(session);
        assert_eq!(spills_containing(MARKER), 0, "drop 删除 spill");

        // finish 路径：同样不留 spill
        let out_path = dir.join("out-finish.txt");
        let mut session = WritebackSession::new(
            WritebackFormat::Txt,
            in_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        session.push("p0-b0", MARKER).unwrap();
        session.finish().unwrap();
        assert_eq!(spills_containing(MARKER), 0, "finish 删除 spill");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空白译文静默忽略（计数不变）；未匹配 id 由回写器忽略（txt 全未匹配
    /// = 逐字节透传）。
    #[test]
    fn blank_translations_and_unmatched_ids_ignored() {
        let dir = case_dir("blank");
        let in_path = dir.join("in.txt");
        let src = "第一章 起\n\n清晨他离开村庄。\n";
        std::fs::write(&in_path, src).unwrap();

        let out_path = dir.join("out.txt");
        let mut session = WritebackSession::new(
            WritebackFormat::Txt,
            in_path.to_str().unwrap(),
            out_path.to_str().unwrap(),
            BiStrategy::BilingualInline,
        )
        .unwrap();
        assert_eq!(session.push("p0-b0", "  \n\t ").unwrap(), 0, "空白忽略");
        assert_eq!(session.push("p0-b0", "").unwrap(), 0, "空串忽略");
        assert_eq!(session.push("p0-b0", "The beginning.").unwrap(), 1);
        assert_eq!(session.push("p0-b123", "bogus").unwrap(), 2, "未匹配 id 仍计数（回写器忽略）");
        session.finish().unwrap();

        // 与「只含一条有效译文」的直接全量调用逐字节一致
        let out_direct = dir.join("out-direct.txt");
        writeback_txt(
            &in_path,
            &pairs_of(&[("p0-b0", "The beginning."), ("p0-b123", "bogus")]),
            BiStrategy::BilingualInline,
            &out_direct,
        )
        .unwrap();
        assert_eq!(std::fs::read(&out_path).unwrap(), std::fs::read(&out_direct).unwrap());
        let out = std::fs::read_to_string(&out_path).unwrap();
        assert!(out.contains("The beginning."), "有效译文写入");
        assert!(out.contains("清晨他离开村庄。"), "未翻译块原文透传");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// feature 关闭时 `new(Docx, …)` 当场报 Unsupported（可行性前置检查）。
    #[test]
    #[cfg(not(feature = "docx"))]
    fn unsupported_when_docx_feature_off() {
        let err = WritebackSession::new(
            WritebackFormat::Docx,
            "in.docx",
            "out.docx",
            BiStrategy::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Unsupported);
        assert!(err.message.contains("docx"), "报错指路 feature：{}", err.message);
    }
}
