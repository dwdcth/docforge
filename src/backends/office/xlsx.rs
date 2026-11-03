// 吸收自 docparser-office 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：crate 引用重写（docparser_core→docforge、office 内部→super）。
//! XLSX 解析：ZIP + quick-xml 流式文本提取。
//!
//! 架构：`XlsxRawBackend` -> `ParseCapability`，支持共享字符串表、样式格式化、日期格式。

use chrono::NaiveDate;
use crate::error::{DocError, Result};
use crate::plugin::{ParseCapability, RawComment, RawDoc, RawImage, RawPage};
use quick_xml::events::Event;
use quick_xml::Reader;

use super::merge::MergeCellPolicy;
use super::zip_read_str;

/// XLSX raw 后端。
#[derive(Default)]
pub struct XlsxRawBackend {
    /// 是否读单元格批注（`xl/comments*.xml` / `threadedComments/*`）。默认关：
    /// 批注是附加信息，默认路径不为它多开一次 ZIP 读（对齐 Python `include_regions`）。
    pub include_regions: bool,
    /// 截断阈值（字节，`0` = 关闭）：文件大于它才启用 `truncate_size` 预算
    /// （对齐 Python `policy.truncate_threshold`）。
    pub truncate_threshold: u64,
    /// 截断预算（字符数，`0` = 关闭）：按行累计，超出即停（对齐 Python `truncate_size`）。
    pub truncate_size: usize,
    /// sheet 上限（`0` = 不限）：超出丢弃且**不**置 `truncated`
    /// （对齐 Python `policy.max_sheets`：列表切片后丢弃）。
    pub max_sheets: usize,
    /// 是否提取内嵌位图（对齐 Python `policy.ocr_images`，默认关）：
    /// sheet → drawing → media，只进 `Page.images`（drawing 里没有正文位置）。
    pub ocr_images: bool,
    /// 合并单元格处置（对齐 Python `policy.merge_cell_policy`，默认 `mark`）。
    pub merge_cell_policy: MergeCellPolicy,
}

impl XlsxRawBackend {
    pub fn new(include_regions: bool) -> Self {
        Self {
            include_regions,
            ..Default::default()
        }
    }

    /// 设置截断预算（`0` = 关闭；语义同 Python `_budget`）。
    pub fn with_truncate(mut self, threshold: u64, size: usize) -> Self {
        self.truncate_threshold = threshold;
        self.truncate_size = size;
        self
    }

    /// 设置 sheet 上限（`0` = 不限；语义同 Python `policy.max_sheets`）。
    pub fn with_max_sheets(mut self, max: usize) -> Self {
        self.max_sheets = max;
        self
    }

    /// 设置是否提取内嵌位图（语义同 Python `policy.ocr_images`）。
    pub fn with_ocr_images(mut self, yes: bool) -> Self {
        self.ocr_images = yes;
        self
    }

    /// 设置合并单元格处置（语义同 Python `policy.merge_cell_policy`）。
    pub fn with_merge_cell_policy(mut self, policy: MergeCellPolicy) -> Self {
        self.merge_cell_policy = policy;
        self
    }
}

impl ParseCapability for XlsxRawBackend {
    fn id(&self) -> &str {
        "office-raw"
    }
    fn formats(&self) -> &[&str] {
        &["xlsx"]
    }
    fn parse(&self, path: &str) -> Result<RawDoc> {
        let mut zip = super::open_zip(path)?;

        // 读取 workbook.xml 获取 sheet 列表
        let wb_xml = zip_read_str(&mut zip, "xl/workbook.xml")?
            .ok_or_else(|| DocError::corrupt("missing xl/workbook.xml"))?;
        let mut sheet_names = parse_sheet_names(&wb_xml)?;
        // sheet 上限：直接丢弃多余 sheet 且**不**置 `truncated`（对齐 Python
        // `sheets = sheets[:max_sheets]`；预算截断才置标志）
        if self.max_sheets > 0 {
            sheet_names.truncate(self.max_sheets);
        }
        // sheet 部件名走 workbook rels（r:id → Target），不要靠 `sheetN.xml` 序号猜：
        // 重命名/重排过的 workbook 用序号会读错表（Python `_read_rels` 同口径）
        let rels = zip_read_str(&mut zip, "xl/_rels/workbook.xml.rels")?
            .map(|x| super::rel_entries(&x))
            .unwrap_or_default();

        // 读取 SharedStrings（惰性）
        let sst = SharedStrings::new(&mut zip)?;

        // 读取 Styles（惰性）
        let styles = Styles::new(&mut zip)?;

        // 截断预算（对齐 Python `_budget`）：文件 > threshold 才启用 size 预算；
        // 预算跨 sheet 累计，命中后当前 sheet 只保留放得下的行，其余 sheet 不再解析
        let budget = super::truncate_budget(path, self.truncate_threshold, self.truncate_size);
        let mut used = 0usize;
        let mut truncated = false;

        // sheet 部件路径先算完：只依赖 workbook rels，不碰 ZIP，便于决定并行策略
        let jobs: Vec<SheetJob> = sheet_names
            .iter()
            .enumerate()
            .map(|(idx, (rid, name))| SheetJob {
                name: name.clone(),
                // rels 命中则用 Target；rel 缺失（手工拼的最小包/损坏包）回退序号命名。
                // 只认 `/worksheet` 关系，`/chartsheet` 等同 r:id 空间里的其它类型不算表。
                part: rels
                    .iter()
                    .find(|(rel_id, rtype, _)| rel_id == rid && rtype.ends_with("/worksheet"))
                    .map(|(_, _, target)| super::resolve_part("xl", target))
                    .unwrap_or_else(|| format!("xl/worksheets/sheet{}.xml", idx + 1)),
            })
            .collect();

        // 解析每个 sheet。
        //
        // 并行只在不启用截断预算时开：预算的语义是「跨 sheet 累计、命中即停」，
        // 并行会把后面的 sheet 也解析掉——输出虽被丢弃，工作量却白做。
        // 每块各自开一次 ZIP：`ZipArchive` 要 `&mut` 才能读条目，跨线程共享它
        // 只能加锁，等于串行（`sst`/`styles` 是纯数据，只读共享）。
        let mut pages: Vec<RawPage> = Vec::with_capacity(jobs.len());
        if budget == 0 && jobs.len() > 1 {
            let contents = super::parallel::map_chunks_ordered(jobs.clone(), |chunk| {
                let mut z = super::open_zip(path)?;
                chunk
                    .iter()
                    .map(|job| {
                        parse_sheet_content(
                            &mut z,
                            &job.part,
                            &sst,
                            &styles,
                            self.include_regions,
                            self.ocr_images,
                            self.merge_cell_policy,
                        )
                    })
                    .collect()
            })?;
            for (job, content) in jobs.iter().zip(contents) {
                pages.push(sheet_page(
                    &job.name,
                    content,
                    &mut used,
                    budget,
                    &mut truncated,
                ));
            }
        } else {
            for job in &jobs {
                let content = parse_sheet_content(
                    &mut zip,
                    &job.part,
                    &sst,
                    &styles,
                    self.include_regions,
                    self.ocr_images,
                    self.merge_cell_policy,
                )?;
                pages.push(sheet_page(
                    &job.name,
                    content,
                    &mut used,
                    budget,
                    &mut truncated,
                ));
                if truncated {
                    break;
                }
            }
        }

        // 与 Python `if not pages: corrupt("未解析到任何 sheet")` 同判据：
        // 零 sheet 的 workbook 是坏包（sheets 元素空 / 全被 rels 过滤掉），
        // 静默返回空结果会让上游把「空跑」当成解析成功。
        // 注意：**有** sheet 但无数据仍是合法文档（空表块 0 字符），不在此拦。
        if pages.is_empty() {
            return Err(DocError::corrupt("workbook 中没有任何 sheet"));
        }

        Ok(RawDoc {
            pages,
            ..Default::default()
        })
    }
}

/// 一个 sheet 的定位信息（只依赖 workbook rels，不依赖 ZIP 内容）。
#[derive(Clone)]
struct SheetJob {
    name: String,
    part: String,
}

/// 单个 sheet 的解析产物。
///
/// 预算与页面组装**刻意留给调用方**：截断是「跨 sheet 累计」的顺序语义，
/// 只有在一处实现才不会出现两条代码路径各算一套。
struct SheetContent {
    rows: Vec<Vec<String>>,
    meta: SheetMeta,
    comments: Vec<RawComment>,
    images: Vec<RawImage>,
}

/// 读一个 sheet：XML → 行；批注/内嵌图按需再读同部件（顺序与 Python 一致）。
#[allow(clippy::too_many_arguments)]
fn parse_sheet_content(
    zip: &mut zip::ZipArchive<std::fs::File>,
    part: &str,
    sst: &SharedStrings,
    styles: &Styles,
    include_regions: bool,
    ocr_images: bool,
    merge_cell_policy: MergeCellPolicy,
) -> Result<SheetContent> {
    let xml = zip_read_str(zip, part)?.unwrap_or_default();
    let parsed = parse_sheet_rows(&xml, sst, styles)?;
    let mut meta = parse_sheet_meta(&xml);
    // 合并单元格处置（默认 `mark`：只标注，不改行数据 → 与历史输出逐字节一致）
    let rows = match merge_cell_policy {
        MergeCellPolicy::Mark => parsed.iter().map(ParsedRow::texts).collect(),
        MergeCellPolicy::Ignore => {
            meta.merges.clear();
            parsed.iter().map(ParsedRow::texts).collect()
        }
        MergeCellPolicy::Expand => expand_merges(&parsed, &meta.merges),
    };
    let comments = if include_regions {
        sheet_comments(zip, part)
    } else {
        Vec::new()
    };
    // 内嵌图（opt-in）：只进 `Page.images`（drawing 里没有正文位置），
    // 与 Python 一样不受 `structure_mode` 影响
    let images = if ocr_images {
        sheet_images(zip, part)
    } else {
        Vec::new()
    };
    Ok(SheetContent {
        rows,
        meta,
        comments,
        images,
    })
}

/// 应用截断预算并组装成页（累计量 `used` / `truncated` 由调用方持有）。
fn sheet_page(
    name: &str,
    content: SheetContent,
    used: &mut usize,
    budget: usize,
    truncated: &mut bool,
) -> RawPage {
    let SheetContent {
        mut rows,
        meta,
        comments,
        images,
    } = content;
    if budget > 0 {
        // 整行放不下就丢该行并停（Python：`used + ln > budget` 触发截断）
        let mut keep = Vec::with_capacity(rows.len());
        for row in rows.drain(..) {
            let cost = row_cost(&row);
            if *used + cost > budget {
                *truncated = true;
                break;
            }
            *used += cost;
            keep.push(row);
        }
        rows = keep;
    }
    // 一个 sheet = 一张表（对齐 Python：空 sheet 仍产出空表块，content 为空）
    let mut item = super::table_item_with_header(rows, meta.header_rows);
    // 与 Python 同形：`merges` 恒存在（可能为空数组），`truncated` 是累计标志
    item.meta.insert("merges".to_string(), meta.merges.into());
    item.meta
        .insert("truncated".to_string(), (*truncated).into());
    RawPage {
        name: Some(name.to_string()),
        items: vec![item],
        comments,
        images,
        ..Default::default()
    }
}

/// 单行在截断预算里的开销：字符数 + 列分隔符（`len-1`）+ 行尾换行 1
/// （逐项对齐 Python `sum(len(c) for c in r) + max(0, len(r) - 1) + 1`）。
fn row_cost(row: &[String]) -> usize {
    let chars: usize = row.iter().map(|c| c.chars().count()).sum();
    chars + row.len().saturating_sub(1) + 1
}

/// 共享字符串表。
struct SharedStrings {
    items: Vec<String>,
}

impl SharedStrings {
    fn new(zip: &mut zip::ZipArchive<std::fs::File>) -> Result<Self> {
        let xml = match zip_read_str(zip, "xl/sharedStrings.xml")? {
            Some(s) => s,
            None => {
                return Ok(SharedStrings { items: Vec::new() });
            }
        };
        let items = parse_sst_items(&xml)?;
        Ok(SharedStrings { items })
    }

    fn get(&self, idx: usize) -> Option<&str> {
        self.items.get(idx).map(|s| s.as_str())
    }
}

/// 解析 `<sst>` 中的 `<si>` 条目。
fn parse_sst_items(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    // 不裁剪文本：空白由消费者 trim（实体两侧空格需保留，见 GeneralRef 分支）
    let mut buf = Vec::new();
    let mut items = Vec::new();
    let mut in_t = false;
    let mut text = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                super::tags::SI => {
                    text.clear();
                }
                super::tags::T => {
                    in_t = true;
                }
                _ => {}
            },
            Ok(Event::End(ref e)) => match e.name().local_name().as_ref() {
                super::tags::SI => {
                    items.push(text.trim().to_string());
                    text.clear();
                }
                super::tags::T => {
                    in_t = false;
                }
                super::tags::SST => {
                    break;
                }
                _ => {}
            },
            Ok(Event::Text(ref e)) => {
                if in_t {
                    // quick-xml 0.38+：Text 已是解码后的字符串，实体需显式 unescape
                    let raw = e.xml10_content();
                    if let Ok(t) = quick_xml::escape::unescape(&raw) {
                        text.push_str(&t);
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_t {
                    super::push_general_ref(&mut text, e);
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_t {
                    text.push_str(e.as_ref());
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("SST parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(items)
}

/// 单元格样式元数据。
#[derive(Default)]
struct Styles {
    num_fmts: std::collections::HashMap<usize, String>,
    xf_num_fmt_ids: Vec<usize>,
}

impl Styles {
    fn new(zip: &mut zip::ZipArchive<std::fs::File>) -> Result<Self> {
        let xml = match zip_read_str(zip, "xl/styles.xml")? {
            Some(s) => s,
            None => return Ok(Styles::default()),
        };
        let mut s = Styles::default();
        s.parse(&xml)?;
        Ok(s)
    }

    fn parse(&mut self, xml: &str) -> Result<()> {
        let mut reader = Reader::from_str(xml);
        // 不裁剪文本：空白由消费者 trim（实体两侧空格需保留，见 GeneralRef 分支）
        let mut buf = Vec::new();
        let mut _in_num_fmts = false;
        let mut _in_cell_xfs = false;
        let mut _num_fmt_id_val: Option<usize> = None;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => match e.name().local_name().as_ref() {
                    super::tags::NUM_FMT => {
                        _num_fmt_id_val = None;
                        let mut fmt_id = 0usize;
                        let mut fmt_code = String::new();
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            match attr.key.as_ref() {
                                "numFmtId" => fmt_id = attr.value.parse().unwrap_or(0),
                                "formatCode" => fmt_code = attr.value.to_string(),
                                _ => {}
                            }
                        }
                        if fmt_id >= 164 {
                            self.num_fmts.insert(fmt_id, fmt_code);
                        }
                    }
                    super::tags::XF => {
                        let mut num_fmt_id = 0usize;
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            if attr.key.as_ref() == "numFmtId" {
                                num_fmt_id = attr.value.parse().unwrap_or(0);
                            }
                        }
                        self.xf_num_fmt_ids.push(num_fmt_id);
                    }
                    _ => {}
                },
                Ok(Event::Empty(ref e)) => match e.name().local_name().as_ref() {
                    super::tags::NUM_FMT => {
                        let mut fmt_id = 0usize;
                        let mut fmt_code = String::new();
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            match attr.key.as_ref() {
                                "numFmtId" => fmt_id = attr.value.parse().unwrap_or(0),
                                "formatCode" => fmt_code = attr.value.to_string(),
                                _ => {}
                            }
                        }
                        if fmt_id >= 164 {
                            self.num_fmts.insert(fmt_id, fmt_code);
                        }
                    }
                    super::tags::XF => {
                        let mut num_fmt_id = 0usize;
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            if attr.key.as_ref() == "numFmtId" {
                                num_fmt_id = attr.value.parse().unwrap_or(0);
                            }
                        }
                        self.xf_num_fmt_ids.push(num_fmt_id);
                    }
                    _ => {}
                },
                Ok(Event::End(ref e)) => {
                    if e.name().local_name().as_ref() == super::tags::CELL_XFS {
                        _in_cell_xfs = false;
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(DocError::corrupt(format!("Styles parse error: {e}"))),
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    }

    /// 判断指定 xf 索引是否为日期格式
    fn is_date_format(&self, xf_idx: Option<usize>) -> bool {
        let num_fmt_id = xf_idx
            .and_then(|i| self.xf_num_fmt_ids.get(i))
            .copied()
            .unwrap_or(0);

        // 内置日期格式 ID: 14-22, 27-36, 45-47, 50-58, 71-81
        matches!(num_fmt_id,
            14..=22 | 27..=36 | 45..=47 | 50..=58 | 71..=81
        ) || self.num_fmts.contains_key(&num_fmt_id)
    }

    /// 获取指定 xf 索引的格式代码字符串（返回 OOXML 格式代码如 "yyyy-mm-dd"）。
    fn code_for(&self, xf_idx: Option<usize>) -> Option<&str> {
        let num_fmt_id = xf_idx
            .and_then(|i| self.xf_num_fmt_ids.get(i))
            .copied()
            .unwrap_or(0);
        if num_fmt_id == 0 {
            return None;
        }
        if let Some(code) = self.num_fmts.get(&num_fmt_id) {
            return Some(code.as_str());
        }
        BUILTIN_FORMATS
            .iter()
            .find(|(id, _)| *id == num_fmt_id)
            .map(|(_, code)| *code)
    }
}

/// OOXML 内置日期格式映射（与 Python ooxml.py BUILTIN_FORMATS 对齐）。
const BUILTIN_FORMATS: &[(usize, &str)] = &[
    (14, "mm-dd-yy"),
    (15, "d-mmm-yy"),
    (16, "d-mmm"),
    (17, "mmm-yy"),
    (18, "h:mm AM/PM"),
    (19, "h:mm:ss AM/PM"),
    (20, "h:mm"),
    (21, "h:mm:ss"),
    (22, "m/d/yy h:mm"),
    (45, "mm:ss"),
    (46, "[h]:mm:ss"),
    (47, "mmss.0"),
];

/// OLE Automation Date 序列号转 NaiveDate。Epoch = Dec 30, 1899。
fn ole_serial_to_date(serial: f64) -> Option<NaiveDate> {
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30)?;
    let mut days = serial as i64;
    // Lotus 123 bug：serial 60 (Feb 29, 1900) 不存在
    if serial > 0.0 && serial < 60.0 {
        days += 1;
    }
    epoch.checked_add_signed(chrono::Duration::days(days))
}

/// 将 OOXML 日期格式代码转换为 strftime 格式。
fn fmt_to_strftime(fmt: &str) -> String {
    let is_12h = fmt.contains("AM/PM");
    let s = fmt
        .replace("yyyy", "%Y")
        .replace("yy", "%y")
        .replace("mmm", "%b")
        .replace("dd", "%d");
    // 区分分钟 mm 与月份 mm（用占位符避免二次替换，与 Python ooxml.py 一致）
    let s = s.replace(":mm", ":M_M").replace("mm:", "M_M:");
    let s = s
        .replace("mm", "=*=")
        .replace("m", "=*=")
        .replace("=*=", "%m");
    let s = s.replace("M_M", "%M");
    let s = s.replace("ss", "%S").replace("AM/PM", "%p");
    if is_12h {
        s.replace("h", "%I")
    } else {
        s.replace("h", "%H")
    }
}

/// 格式化 Excel 日期（OLE 序列号 + 格式代码 → 字符串）。
fn format_excel_date(serial: f64, format_code: &str) -> String {
    if let Some(date) = ole_serial_to_date(serial) {
        let strfmt = fmt_to_strftime(format_code);
        date.format(&strfmt).to_string()
    } else {
        format!("num:{}", serial)
    }
}

/// 格式化数字：去科学记数法、去前导零，与 Python 的 `str()` 一致。
fn format_number(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(num) = trimmed.parse::<f64>() {
        if num.fract() == 0.0 && num.is_finite() {
            format!("{}", num as i64)
        } else {
            format!("{}", num)
        }
    } else {
        trimmed.to_string()
    }
}

/// 解析 workbook.xml 获取 sheet 名称列表。
fn parse_sheet_names(xml: &str) -> Result<Vec<(String, String)>> {
    let mut reader = Reader::from_str(xml);
    // 不裁剪文本：空白由消费者 trim（实体两侧空格需保留，见 GeneralRef 分支）
    let mut buf = Vec::new();
    let mut sheets = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().local_name().as_ref() == "sheet" {
                    let mut rid = String::new();
                    let mut name = String::new();
                    for attr in e.attributes().filter_map(|a| a.ok()) {
                        // 属性按完整 QName 比较（`r:id` 与裸 `id` 区分，与迁移前一致）
                        match attr.key.as_ref() {
                            "r:id" => rid = attr.value.to_string(),
                            "name" => name = attr.value.to_string(),
                            // 某些 xlsx 使用短 id 属性
                            "id" if rid.is_empty() => rid = attr.value.to_string(),
                            _ => {}
                        }
                    }
                    if !name.is_empty() {
                        sheets.push((rid, name));
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().local_name().as_ref() == "workbook" {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("workbook parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(sheets)
}

/// sheet 级元信息（对齐 Python `_read_sheet` 返回的 `(merges, frozen)`）。
#[derive(Debug, Default, PartialEq)]
struct SheetMeta {
    /// 前 N 行为表头（0 = 未识别）
    header_rows: usize,
    /// 合并区域（`A1:B2`），保持文档出现顺序
    merges: Vec<String>,
}

/// 一次扫描取 sheet 元信息：冻结窗格 → 表头行，`<mergeCell ref>` → 合并区域。
///
/// 与 Python `_read_sheet` 同判据：只认 `<pane state="frozen" ySplit="N">` 且 `N > 0`，
/// 结果是 1 行表头（不做「冻结 3 行 = 3 行表头」的细分——Python 也只给 1）。
/// 解析失败一律返回空元信息：表头/合并标注是锦上添花，不该把整张表拖成 corrupt
/// （sheet 本身的语法错误由 `parse_sheet_rows` 报）。
fn parse_sheet_meta(xml: &str) -> SheetMeta {
    let mut meta = SheetMeta::default();
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.name().local_name().as_ref() {
                    "pane" => {
                        let mut state = String::new();
                        let mut y_split = 0i64;
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            match attr.key.as_ref() {
                                "state" => state = attr.value.to_string(),
                                "ySplit" => y_split = attr.value.parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                        if state == "frozen" && y_split > 0 {
                            meta.header_rows = 1;
                        }
                    }
                    "mergeCell" => {
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            if attr.key.as_ref() == "ref" && !attr.value.is_empty() {
                                meta.merges.push(attr.value.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    meta
}

/// 单元格批注：`(单元格引用, 文本)`，按批注部件文档序。
///
/// 优先现代 `threadedComments`（Excel 2019+ 的事实来源，`comments1.xml` 只是兼容镜像）；
/// 两份都读会把同一条批注算两次，故先命中先返回。部件缺失/损坏只丢该 sheet 的批注，
/// 不阻断整表（批注是附加信息）。
fn sheet_comments(zip: &mut zip::ZipArchive<std::fs::File>, sheet_part: &str) -> Vec<RawComment> {
    let Ok(Some(rels)) = zip_read_str(zip, &super::part_rels_path(sheet_part)) else {
        return Vec::new();
    };
    let rels = super::rel_entries(&rels);
    let dir = super::part_dir(sheet_part);

    for (needle, threaded) in [("threadedcomment", true), ("/comments", false)] {
        for (_id, rtype, target) in &rels {
            if !rtype.to_ascii_lowercase().contains(needle) {
                continue;
            }
            let part = super::resolve_part(dir, target);
            let Ok(Some(xml)) = zip_read_str(zip, &part) else {
                continue;
            };
            let got = if threaded {
                threaded_comment_texts(&xml)
            } else {
                legacy_comment_texts(&xml)
            };
            if let Ok(list) = got {
                if !list.is_empty() {
                    return list;
                }
            }
        }
    }
    Vec::new()
}

/// sheet 内嵌图：`sheet → drawing → media`（对齐 Python `_sheet_images`）。
///
/// 与 docx/pptx 的差别只有去重域：这里**不去重**——Python 对每张 drawing 的每个
/// blip 各自产一项，同一 media 被两个 drawing 引用就会出两条（Rust 照抄该口径）。
/// drawing 部件/rels 缺失或损坏只跳过那一块：图片是附加信息，不该拖垮整表。
fn sheet_images(zip: &mut zip::ZipArchive<std::fs::File>, sheet_part: &str) -> Vec<RawImage> {
    let Ok(Some(rels)) = zip_read_str(zip, &super::part_rels_path(sheet_part)) else {
        return Vec::new();
    };
    let mut out: Vec<RawImage> = Vec::new();
    let sheet_dir = super::part_dir(sheet_part);
    for (_id, rtype, target) in super::rel_entries(&rels) {
        if !rtype.to_ascii_lowercase().contains("drawing") {
            continue;
        }
        let drawing = super::resolve_part(sheet_dir, &target);
        let Ok(Some(drawing_xml)) = zip_read_str(zip, &drawing) else {
            continue;
        };
        let drels = zip_read_str(zip, &super::part_rels_path(&drawing))
            .ok()
            .flatten()
            .map(|x| super::rel_entries(&x))
            .unwrap_or_default();
        let drawing_dir = super::part_dir(&drawing);
        for rid in super::image::blip_rids(&drawing_xml) {
            let Some((_, _, media)) = drels.iter().find(|(id, _, _)| *id == rid) else {
                continue;
            };
            let part = super::resolve_part(drawing_dir, media);
            if let Some(im) = super::image::make_image(zip, &part) {
                out.push(im);
            }
        }
    }
    out
}

/// legacy 批注部件（`xl/comments1.xml`）：`<comment ref="A1"><text><r><t>…</t></r></text></comment>`。
///
/// 文本取 `<text>` 下所有 `<t>` 拼接（phonetic `rPh` 不剔除，两侧同口径）；
/// `ref` 为空或文本为空（只有署名/空批注）的条目跳过。
fn legacy_comment_texts(xml: &str) -> Result<Vec<RawComment>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawComment> = Vec::new();
    let mut cell = String::new();
    let mut text = String::new();
    let mut in_comment = false;
    let mut in_t = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.name().local_name().as_ref() {
                    "comment" => {
                        in_comment = true;
                        cell = super::attr_by(e, "ref")
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        text.clear();
                    }
                    "t" => in_t = true,
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => match e.name().local_name().as_ref() {
                "comment" => {
                    in_comment = false;
                    let trimmed = text.trim();
                    if !trimmed.is_empty() && !cell.is_empty() {
                        out.push(RawComment {
                            cell: cell.clone(),
                            text: trimmed.to_string(),
                        });
                    }
                    text.clear();
                }
                "t" => in_t = false,
                _ => {}
            },
            Ok(Event::Text(ref e)) => {
                if in_comment && in_t {
                    if let Ok(t) = quick_xml::escape::unescape(&e.xml10_content()) {
                        text.push_str(&t);
                    }
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_comment && in_t {
                    text.push_str(e.as_ref());
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML comment parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// threaded 批注部件（`xl/threadedComments/threadedComment1.xml`）：
/// `<threadedComment ref="A1" …><text>…</text></threadedComment>`，文本直接取 `<text>` 内字符数据。
fn threaded_comment_texts(xml: &str) -> Result<Vec<RawComment>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawComment> = Vec::new();
    let mut cell = String::new();
    let mut text = String::new();
    let mut in_comment = false;
    let mut in_text = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.name().local_name().as_ref() {
                    "threadedComment" => {
                        in_comment = true;
                        cell = super::attr_by(e, "ref")
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        text.clear();
                    }
                    "text" => in_text = true,
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => match e.name().local_name().as_ref() {
                "threadedComment" => {
                    in_comment = false;
                    let trimmed = text.trim();
                    if !trimmed.is_empty() && !cell.is_empty() {
                        out.push(RawComment {
                            cell: cell.clone(),
                            text: trimmed.to_string(),
                        });
                    }
                    text.clear();
                }
                "text" => in_text = false,
                _ => {}
            },
            Ok(Event::Text(ref e)) => {
                if in_comment && in_text {
                    if let Ok(t) = quick_xml::escape::unescape(&e.xml10_content()) {
                        text.push_str(&t);
                    }
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_comment && in_text {
                    text.push_str(e.as_ref());
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("XML comment parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// 一行原始数据：`number` 是 0-based **原始行号**，`cells` 是稀疏列（列号 → 文本）。
///
/// 保留行号与列号是 `merge_cell_policy = expand` 的前提：合并区用 `B2:D3`
/// 这种绝对坐标定位，压缩成紧凑数组后位置信息就没了。默认 `mark` 只取
/// [`ParsedRow::texts`]，与历史输出逐字节一致。
struct ParsedRow {
    number: usize,
    /// 按列号升序（`(列号, 文本)`，空单元格不出现）
    cells: Vec<(usize, String)>,
}

impl ParsedRow {
    /// 紧凑文本（丢空单元格，历史行为）
    fn texts(&self) -> Vec<String> {
        self.cells.iter().map(|(_, t)| t.clone()).collect()
    }
}

/// `<row r="N">` → 0-based 行号（`N - 1`）；缺 `r` 或解析失败 → `None`。
fn row_number(e: &quick_xml::events::BytesStart<'_>) -> Option<usize> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == "r")
        .and_then(|a| a.value.parse::<usize>().ok())
        .and_then(|n| n.checked_sub(1))
}

/// 合并区展开：把左上角文本铺满整个区域，并把行稠密成**按列号对齐**的矩形。
///
/// * 行号用原始 `r=` 序号（空行被跳过但序号不回收），所以 `B2:D3` 能定位到正确的行；
/// * 稠密化是必须的：紧凑数组里「第 2 个值」在不同行对应不同列，铺出来的值会错列；
/// * 每行只裁**尾部**空列，位置语义不变，又不会给每行挂一长串空串（截断预算按行累计）。
/// * 左上角为空（或该行被跳过）就不铺——填空串没有信息增量，还会把空格子变多。
fn expand_merges(rows: &[ParsedRow], merges: &[String]) -> Vec<Vec<String>> {
    use std::collections::BTreeMap;

    let mut dense: Vec<BTreeMap<usize, String>> = rows
        .iter()
        .map(|r| r.cells.iter().cloned().collect())
        .collect();

    for m in merges {
        let Some(((r1, c1), (r2, c2))) = super::merge::parse_merge_ref(m) else {
            continue;
        };
        let Some(top) = rows
            .iter()
            .position(|r| r.number == r1)
            .and_then(|i| dense[i].get(&c1).cloned())
            .filter(|t| !t.trim().is_empty())
        else {
            continue;
        };
        for (i, row) in rows.iter().enumerate() {
            if row.number < r1 || row.number > r2 {
                continue;
            }
            for c in c1..=c2 {
                dense[i].entry(c).or_insert_with(|| top.clone());
            }
        }
    }

    // 全局最大列 + 1：矩形宽度按整个 sheet 算，才能保证列号 ↔ 下标一一对应
    let width = dense
        .iter()
        .flat_map(|m| m.keys().copied())
        .max()
        .map_or(0, |c| c + 1);
    dense
        .into_iter()
        .map(|m| {
            let mut row = vec![String::new(); width];
            for (c, t) in m {
                if let Some(slot) = row.get_mut(c) {
                    *slot = t;
                }
            }
            while row.last().is_some_and(|t| t.is_empty()) {
                row.pop();
            }
            row
        })
        .collect()
}

/// 解析 sheet 中的行/列单元格。
fn parse_sheet_rows(xml: &str, sst: &SharedStrings, styles: &Styles) -> Result<Vec<ParsedRow>> {
    let mut reader = Reader::from_str(xml);
    // 不裁剪文本：空白由消费者 trim（实体两侧空格需保留，见 GeneralRef 分支）
    let mut buf = Vec::new();
    let mut rows: Vec<ParsedRow> = Vec::new();
    let mut current_row: Vec<(usize, String)> = Vec::new(); // (col_idx, text)
                                                            // 行号：优先 `<row r="N">`，缺 `r` 时按出现顺序补位。空行不产出行，
                                                            // 但序号必须继续走（否则后面的合并区会整体错行）
    let mut row_seq = 0usize;
    let mut row_ref: Option<usize> = None;
    let mut cell_ref = String::new();
    let mut cell_type = 's'; // s = shared string, n = number, b = boolean, e = error, str = inline
    let mut cell_style: Option<usize> = None;
    let mut cell_value = String::new();
    let mut in_v = false;
    let mut in_is = false;
    let mut in_is_t = false;

    // regex 用于解析列号：如 "A1" -> 0, "Z1" -> 25, "AA1" -> 26
    fn col_from_ref(r: &str) -> usize {
        let letters: String = r.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let mut col = 0usize;
        for c in letters.chars() {
            col = col * 26 + (c.to_ascii_uppercase() as u8 - b'A' + 1) as usize;
        }
        if col > 0 {
            col - 1
        } else {
            0
        }
    }

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match e.name().local_name().as_ref() {
                    super::tags::ROW => row_ref = row_number(e),
                    super::tags::C => {
                        cell_type = 's';
                        cell_style = None;
                        cell_value.clear();
                        cell_ref.clear();
                        for attr in e.attributes().filter_map(|a| a.ok()) {
                            match attr.key.as_ref() {
                                "r" => cell_ref = attr.value.to_string(),
                                "t" => {
                                    // t: s=shared string, n=number, b=boolean, e=error, str=inlineStr
                                    cell_type = match attr.value.as_ref() {
                                        "s" => 's',                 // shared string
                                        "n" => 'n',                 // number
                                        "b" => 'b',                 // boolean
                                        "e" => 'e',                 // error
                                        "str" | "inlineStr" => 'i', // inline string
                                        _ => 's',
                                    };
                                }
                                "s" => cell_style = attr.value.parse().ok(),
                                _ => {}
                            }
                        }
                    }
                    super::tags::V => {
                        in_v = true;
                    }
                    super::tags::IS => {
                        in_is = true;
                    }
                    super::tags::T if in_is => in_is_t = true,
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                match e.name().local_name().as_ref() {
                    super::tags::C => {
                        // 处理单元格值
                        let text = match cell_type {
                            's' => {
                                let idx: usize = cell_value.trim().parse().unwrap_or(0);
                                sst.get(idx).unwrap_or("").to_string()
                            }
                            'i' => {
                                // inline string: <is><t>...</t></is>，文本已在 cell_value 中
                                cell_value.trim().to_string()
                            }
                            'b' => {
                                // 匹配 Python: True/False 而非 TRUE/FALSE
                                if cell_value.trim() == "1" {
                                    "True".to_string()
                                } else {
                                    "False".to_string()
                                }
                            }
                            'e' => {
                                // 匹配 Python: 前缀 "ERROR:"
                                format!("ERROR:{}", cell_value.trim())
                            }
                            // 'n' or default: 纯数字或日期
                            _ => {
                                let val = cell_value.trim();
                                if val.is_empty() {
                                    String::new()
                                } else if styles.is_date_format(cell_style) {
                                    // 日期：按格式代码格式化
                                    if let Ok(num) = val.parse::<f64>() {
                                        if let Some(code) = styles.code_for(cell_style) {
                                            format_excel_date(num, code)
                                        } else {
                                            format!("num:{}", val)
                                        }
                                    } else {
                                        format!("num:{}", val)
                                    }
                                } else {
                                    // 数值：去科学记数法、去前导零
                                    format_number(val)
                                }
                            }
                        };

                        // `<c>` 省略 `r` 时按 Excel 规范「紧跟上一个单元格」定列：
                        // 不能一律当 0 列——紧凑输出（`texts`）靠稳定排序看不出问题，
                        // 但 `merge_cell_policy = expand` 要按列号定位，全 0 会互相覆盖
                        let col = if cell_ref.is_empty() {
                            current_row.last().map_or(0, |(c, _)| c + 1)
                        } else {
                            col_from_ref(&cell_ref)
                        };
                        current_row.push((col, text));
                        in_v = false;
                        in_is = false;
                        in_is_t = false;
                    }
                    super::tags::ROW => {
                        // 排序并输出行；整行空白不产出记录
                        // （对齐 Python `_read_sheet`：`if any(c.strip() for c in cur)`）
                        let number = row_ref.take().unwrap_or(row_seq);
                        row_seq = number.saturating_add(1);
                        current_row.sort_by_key(|(col, _)| *col);
                        if current_row.iter().any(|(_, t)| !t.trim().is_empty()) {
                            rows.push(ParsedRow {
                                number,
                                cells: std::mem::take(&mut current_row),
                            });
                        }
                        current_row.clear();
                    }
                    super::tags::V => {
                        in_v = false;
                    }
                    super::tags::IS => {
                        in_is = false;
                    }
                    super::tags::T if in_is_t => in_is_t = false,
                    _ => {}
                }
            }
            // `<row r="5"/>`：无单元格的空行不产出记录，但序号要跟着走
            Ok(Event::Empty(ref e)) => {
                if e.name().local_name().as_ref() == super::tags::ROW {
                    if let Some(n) = row_number(e) {
                        row_seq = n.saturating_add(1);
                    }
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_v || in_is_t {
                    let raw = e.xml10_content();
                    if let Ok(t) = quick_xml::escape::unescape(&raw) {
                        cell_value.push_str(&t);
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                if in_v || in_is_t {
                    super::push_general_ref(&mut cell_value, e);
                }
            }
            Ok(Event::CData(ref e)) => {
                if in_v || in_is_t {
                    cell_value.push_str(e.as_ref());
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(DocError::corrupt(format!("sheet parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    // 收尾：XML 被截断时最后一行可能没有 `</row>`（Python 同样补一次）
    current_row.sort_by_key(|(col, _)| *col);
    if current_row.iter().any(|(_, t)| !t.trim().is_empty()) {
        let number = row_ref.take().unwrap_or(row_seq);
        rows.push(ParsedRow {
            number,
            cells: current_row,
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sst_empty() {
        let sst = SharedStrings { items: vec![] };
        assert_eq!(sst.get(0), None);
    }

    #[test]
    fn test_sst_parse() {
        let xml = r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="2">
          <si><t>Hello</t></si>
          <si><t>World</t></si>
        </sst>"#;
        let items = parse_sst_items(xml).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], "Hello");
        assert_eq!(items[1], "World");
    }

    /// 整行空白的记录不产出（对齐 Python `if any(c.strip() for c in cur)`）。
    #[test]
    fn test_sheet_skips_blank_rows() {
        let xml = concat!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>甲</t></is></c><c r="B1" t="inlineStr"><is><t>乙</t></is></c></row>"#,
            r#"<row r="2"><c r="A2"/><c r="B2"/></row>"#,
            r#"<row r="3"><c r="A3" t="inlineStr"><is><t> </t></is></c></row>"#,
            r#"<row r="4"><c r="A4" t="inlineStr"><is><t>丙</t></is></c></row>"#,
            r#"</sheetData></worksheet>"#
        );
        let sst = SharedStrings { items: vec![] };
        let rows = parse_sheet_rows(xml, &sst, &Styles::default()).unwrap();
        let texts: Vec<Vec<String>> = rows.iter().map(ParsedRow::texts).collect();
        assert_eq!(
            texts,
            vec![
                vec!["甲".to_string(), "乙".to_string()],
                vec!["丙".to_string()],
            ]
        );
        // 行号不回收：第 2/3 行被跳过，第 4 行仍是 3（`A4` 的合并区才对得上）
        let numbers: Vec<usize> = rows.iter().map(|r| r.number).collect();
        assert_eq!(numbers, vec![0, 3]);
    }

    #[test]
    fn test_sheet_names() {
        let xml = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
          <sheets>
            <sheet name="Sheet1" r:id="rId1"/>
            <sheet name="Sheet2" r:id="rId2"/>
          </sheets>
        </workbook>"#;
        let sheets = parse_sheet_names(xml).unwrap();
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].1, "Sheet1");
    }

    /// reader 迁移 smoke：最小 xlsx（workbook + sharedStrings + sheet），走完整 ZIP → 事件流路径。
    ///
    /// 覆盖 `local_name()` 元素匹配、`attr.value`（Cow<str>）取值与共享字符串实体解码。
    #[test]
    fn smoke_parse_minimal_xlsx_zip() {
        use std::io::Write;

        const WORKBOOK: &str = concat!(
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<sheets><sheet name="S1" sheetId="1" r:id="rId1"/></sheets></workbook>"#
        );
        const SST: &str = concat!(
            r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
            r#"<si><t>甲</t></si><si><t>乙 &amp; 丙</t></si></sst>"#
        );
        const SHEET: &str = concat!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
            r#"<sheetViews><sheetView workbookViewId="0">"#,
            r#"<pane ySplit="1" topLeftCell="A2" state="frozen"/>"#,
            r#"</sheetView></sheetViews><sheetData><row r="1">"#,
            r#"<c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c>"#,
            r#"</row></sheetData></worksheet>"#
        );

        let dir = std::env::temp_dir().join("docparser_office_xlsx_smoke");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("min.xlsx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("xl/workbook.xml", WORKBOOK),
                ("xl/sharedStrings.xml", SST),
                ("xl/worksheets/sheet1.xml", SHEET),
            ] {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }

        let raw = XlsxRawBackend::default()
            .parse(path.to_str().unwrap())
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(raw.pages.len(), 1);
        assert_eq!(raw.pages[0].name.as_deref(), Some("S1"));
        assert_eq!(raw.pages[0].items.len(), 1);
        assert_eq!(raw.pages[0].items[0].text, "甲,乙 & 丙");
        // 冻结窗格必须穿透到 TableData（assemble 原样透传，Block.table.header_rows=1）
        let table = raw.pages[0].items[0].table.as_ref().expect("缺 table");
        assert_eq!(
            table.rows,
            vec![vec!["甲".to_string(), "乙 & 丙".to_string()]]
        );
        assert_eq!(table.header_rows, 1);
        // 元信息也必须穿透（Python 侧同样挂 merges/truncated，空表也要有键）
        assert_eq!(
            raw.pages[0].items[0].meta.get("merges"),
            Some(&crate::model::MetaValue::List(vec![]))
        );
        assert_eq!(
            raw.pages[0].items[0].meta.get("truncated"),
            Some(&crate::model::MetaValue::Bool(false))
        );
    }

    /// 冻结首行 → `header_rows = 1`（与 Python 判据一致）。
    #[test]
    fn frozen_pane_marks_header_row() {
        let xml = concat!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
            r#"<sheetViews><sheetView workbookViewId="0">"#,
            r#"<pane xSplit="0" ySplit="1" topLeftCell="A2" activePane="bottomLeft" state="frozen"/>"#,
            r#"</sheetView></sheetViews><sheetData/></worksheet>"#
        );
        assert_eq!(parse_sheet_meta(xml).header_rows, 1);
    }

    /// 未冻结 / 只冻列 / ySplit 非正 → 不算表头。
    #[test]
    fn unfrozen_pane_is_not_header() {
        let cases = [
            // 无 state（仅拆分窗格）
            r#"<worksheet><sheetViews><sheetView><pane ySplit="1"/></sheetView></sheetViews></worksheet>"#,
            // 只冻结列
            r#"<worksheet><sheetViews><sheetView><pane xSplit="2" ySplit="0" state="frozen"/></sheetView></sheetViews></worksheet>"#,
            // 坏属性值
            r#"<worksheet><sheetViews><sheetView><pane ySplit="abc" state="frozen"/></sheetView></sheetViews></worksheet>"#,
            // 完全没有 sheetViews
            r#"<worksheet><sheetData/></worksheet>"#,
        ];
        for xml in cases {
            assert_eq!(parse_sheet_meta(xml).header_rows, 0, "{xml}");
        }
    }

    /// `<mergeCell ref>` 按文档顺序收进 `merges`；缺 ref / 空 ref 忽略。
    #[test]
    fn merge_cells_collected_in_document_order() {
        let xml = concat!(
            r#"<worksheet><sheetData/><mergeCells count="3">"#,
            r#"<mergeCell ref="B1:C1"/><mergeCell ref="A2:A3"/>"#,
            r#"<mergeCell ref=""/><mergeCell/>"#,
            r#"</mergeCells></worksheet>"#
        );
        assert_eq!(
            parse_sheet_meta(xml).merges,
            vec!["B1:C1".to_string(), "A2:A3".to_string()]
        );
    }

    /// 没有 `mergeCells` → 空列表（不是 None：Python 侧写 `or []` 后同样是 `[]`）。
    #[test]
    fn no_merge_cells_yields_empty_list() {
        assert!(parse_sheet_meta(r#"<worksheet><sheetData/></worksheet>"#)
            .merges
            .is_empty());
    }

    /// `expand`：左上值铺满合并区，行按**列号**稠密对齐；
    /// 空行不产出但行号不回收（`A2:A4` 仍命中第 4 行）。
    #[test]
    fn expand_fills_merge_area_and_aligns_columns() {
        let xml = concat!(
            r#"<worksheet><sheetData>"#,
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>地区</t></is></c>"#,
            r#"<c r="B1" t="inlineStr"><is><t>销量</t></is></c></row>"#,
            r#"<row r="2"><c r="A2" t="inlineStr"><is><t>华东</t></is></c>"#,
            r#"<c r="B2" t="inlineStr"><is><t>10</t></is></c></row>"#,
            r#"<row r="3"/>"#,
            r#"<row r="4"><c r="B4" t="inlineStr"><is><t>20</t></is></c></row>"#,
            r#"</sheetData><mergeCells count="1"><mergeCell ref="A2:A4"/></mergeCells></worksheet>"#
        );
        let sst = SharedStrings { items: vec![] };
        let styles = Styles::default();
        let parsed = parse_sheet_rows(xml, &sst, &styles).unwrap();
        let meta = parse_sheet_meta(xml);
        // 默认 `mark`：行保持紧凑（历史行为），`A4` 没有值就是没有
        let compact: Vec<Vec<String>> = parsed.iter().map(ParsedRow::texts).collect();
        assert_eq!(
            compact,
            vec![
                vec!["地区".to_string(), "销量".to_string()],
                vec!["华东".to_string(), "10".to_string()],
                vec!["20".to_string()],
            ]
        );
        // `expand`：第 4 行补上 A 列的「华东」，且与 B 列对齐（不是挤到索引 0）
        assert_eq!(
            expand_merges(&parsed, &meta.merges),
            vec![
                vec!["地区".to_string(), "销量".to_string()],
                vec!["华东".to_string(), "10".to_string()],
                vec!["华东".to_string(), "20".to_string()],
            ]
        );
    }

    /// 省略 `r` 属性（`<row>` / `<c>` 都没有 `r`）时列号按出现顺序定。
    ///
    /// 曾经一律当 0 列：紧凑输出（`texts` 靠稳定排序）看不出问题，但 `expand`
    /// 按列号定位时同行单元格会互相覆盖——`xlsx_rich.xlsx` 这类手造 fixture
    /// 就是把「姓名/张三」压成一个值的（0.2.0 发布后实测发现）。
    #[test]
    fn expand_handles_cells_without_ref() {
        let xml = concat!(
            r#"<worksheet><sheetData>"#,
            r#"<row><c t="inlineStr"><is><t>姓名</t></is></c>"#,
            r#"<c t="inlineStr"><is><t>张三</t></is></c></row>"#,
            r#"<row><c t="inlineStr"><is><t>李四</t></is></c>"#,
            r#"<c t="inlineStr"><is><t>100</t></is></c></row>"#,
            r#"</sheetData><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#
        );
        let sst = SharedStrings { items: vec![] };
        let styles = Styles::default();
        let parsed = parse_sheet_rows(xml, &sst, &styles).unwrap();
        let meta = parse_sheet_meta(xml);
        // 列号：0、1（不是 0、0）
        assert_eq!(
            parsed[0].cells,
            vec![(0, "姓名".to_string()), (1, "张三".to_string())]
        );
        assert_eq!(
            parsed[1].cells,
            vec![(0, "李四".to_string()), (1, "100".to_string())]
        );
        // mark：紧凑输出不变（历史行为与 Python 一致）
        let compact: Vec<Vec<String>> = parsed.iter().map(ParsedRow::texts).collect();
        assert_eq!(
            compact,
            vec![
                vec!["姓名".to_string(), "张三".to_string()],
                vec!["李四".to_string(), "100".to_string()],
            ]
        );
        // expand：A1:B1 已铺满，两行都保持两列——修复前同一行的值会因列号全 0
        // 互相覆盖（第 2 行只剩 ["100"]）
        assert_eq!(expand_merges(&parsed, &meta.merges), compact);
    }

    /// 三态穿透到 `RawPage`：`mark` 标注、`ignore` 清空标注、`expand` 填值，行数据只在
    /// `expand` 时变。默认（不带选项）必须与历史输出一致。
    #[test]
    fn merge_cell_policy_three_states() {
        use std::io::Write;

        const WORKBOOK: &str = concat!(
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<sheets><sheet name="S1" sheetId="1" r:id="rId1"/></sheets></workbook>"#
        );
        const WB_RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
            r#"</Relationships>"#
        );
        const SHEET: &str = concat!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>地区</t></is></c>"#,
            r#"<c r="B1" t="inlineStr"><is><t>销量</t></is></c></row>"#,
            r#"<row r="2"><c r="A2" t="inlineStr"><is><t>华东</t></is></c>"#,
            r#"<c r="B2" t="inlineStr"><is><t>10</t></is></c></row>"#,
            r#"<row r="3"><c r="B3" t="inlineStr"><is><t>20</t></is></c></row>"#,
            r#"</sheetData><mergeCells count="1"><mergeCell ref="A2:A3"/></mergeCells></worksheet>"#
        );

        let dir = std::env::temp_dir().join("docparser_office_xlsx_merge_policy");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("merge.xlsx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("xl/workbook.xml", WORKBOOK),
                ("xl/_rels/workbook.xml.rels", WB_RELS),
                ("xl/worksheets/sheet1.xml", SHEET),
            ] {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        let path = path.to_str().unwrap().to_string();

        let rows_of = |raw: &crate::plugin::RawDoc| -> Vec<Vec<String>> {
            raw.pages[0].items[0]
                .table
                .as_ref()
                .expect("缺 table")
                .rows
                .clone()
        };
        let merges_of = |raw: &crate::plugin::RawDoc| -> crate::model::MetaValue {
            raw.pages[0].items[0]
                .meta
                .get("merges")
                .cloned()
                .expect("缺 merges 元信息")
        };
        let marked = crate::model::MetaValue::List(vec!["A2:A3".to_string()]);

        // 默认（不带选项）= mark：只标注，行不动
        let default_raw = XlsxRawBackend::default().parse(&path).unwrap();
        assert_eq!(
            rows_of(&default_raw),
            vec![
                vec!["地区".to_string(), "销量".to_string()],
                vec!["华东".to_string(), "10".to_string()],
                vec!["20".to_string()],
            ]
        );
        assert_eq!(merges_of(&default_raw), marked);

        // mark 显式指定 → 同上
        let mark_raw = XlsxRawBackend::default()
            .with_merge_cell_policy(MergeCellPolicy::Mark)
            .parse(&path)
            .unwrap();
        assert_eq!(rows_of(&mark_raw), rows_of(&default_raw));

        // ignore：标注清空（仍是列表，与 Python `or []` 同形），行不动
        let ignore_raw = XlsxRawBackend::default()
            .with_merge_cell_policy(MergeCellPolicy::Ignore)
            .parse(&path)
            .unwrap();
        assert_eq!(
            merges_of(&ignore_raw),
            crate::model::MetaValue::List(vec![])
        );
        assert_eq!(rows_of(&ignore_raw), rows_of(&default_raw));

        // expand：第 3 行补上 A 列，标注保留（下游仍知道这块是合并来的）
        let expand_raw = XlsxRawBackend::default()
            .with_merge_cell_policy(MergeCellPolicy::Expand)
            .parse(&path)
            .unwrap();
        assert_eq!(
            rows_of(&expand_raw),
            vec![
                vec!["地区".to_string(), "销量".to_string()],
                vec!["华东".to_string(), "10".to_string()],
                vec!["华东".to_string(), "20".to_string()],
            ]
        );
        assert_eq!(merges_of(&expand_raw), marked);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_col_ref() {
        fn col(r: &str) -> usize {
            let letters: String = r.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
            let mut col = 0usize;
            for c in letters.chars() {
                col = col * 26 + (c.to_ascii_uppercase() as u8 - b'A' + 1) as usize;
            }
            if col > 0 {
                col - 1
            } else {
                0
            }
        }
        assert_eq!(col("A1"), 0);
        assert_eq!(col("Z1"), 25);
        assert_eq!(col("AA1"), 26);
        assert_eq!(col("AB1"), 27);
    }

    /// legacy 批注：多 run 拼接、空白批注与缺 ref 的条目丢弃（与 Python 同口径）。
    #[test]
    fn legacy_comments_join_runs_and_skip_blank() {
        let xml = concat!(
            r#"<comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
            r#"<authors><author>评审</author></authors><commentList>"#,
            r#"<comment ref="A2" authorId="0"><text><r><t>批注一</t></r><r><t>，第二段</t></r></text></comment>"#,
            r#"<comment ref="B3" authorId="0"><text><r><t> </t></r></text></comment>"#,
            r#"<comment ref="" authorId="0"><text><r><t>无引用</t></r></text></comment>"#,
            r#"<comment ref="C4" authorId="0"><text></text></comment>"#,
            r#"</commentList></comments>"#
        );
        let got = legacy_comment_texts(xml).unwrap();
        let got: Vec<(&str, &str)> = got
            .iter()
            .map(|c| (c.cell.as_str(), c.text.as_str()))
            .collect();
        assert_eq!(got, [("A2", "批注一，第二段")]);
    }

    /// threaded 批注：文本取 `<text>` 内字符数据并 trim。
    #[test]
    fn threaded_comments_take_text_element() {
        let xml = concat!(
            r#"<ThreadedComments xmlns="http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments">"#,
            r#"<threadedComment ref="A1" id="{c1}"><text> 线程批注 </text></threadedComment>"#,
            r#"<threadedComment ref="B2" id="{c2}"><text/></threadedComment>"#,
            r#"</ThreadedComments>"#
        );
        let got = threaded_comment_texts(xml).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].cell, "A1");
        assert_eq!(got[0].text, "线程批注");
    }

    /// 批注端到端：只在 opt-in 时读；threaded 优先于 legacy 镜像（不重复计数）。
    #[test]
    fn comments_read_only_when_opted_in() {
        use std::io::Write;

        const WORKBOOK: &str = concat!(
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<sheets><sheet name="S1" sheetId="1" r:id="rId1"/></sheets></workbook>"#
        );
        const WB_RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
            r#"</Relationships>"#
        );
        const SHEET: &str = concat!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
            r#"<sheetData><row><c t="inlineStr"><is><t>甲</t></is></c></row></sheetData></worksheet>"#
        );
        // sheet 级 rels：threaded 与 legacy 同时存在，target 用 `../` 相对路径
        const SHEET_RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.xml"/>"#,
            r#"<Relationship Id="rId1" Type="http://schemas.microsoft.com/office/2017/10/relationships/threadedComment" Target="../threadedComments/threadedComment1.xml"/>"#,
            r#"</Relationships>"#
        );
        const LEGACY: &str = concat!(
            r#"<comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
            r#"<commentList><comment ref="A1"><text><r><t>镜像文本</t></r></text></comment></commentList></comments>"#
        );
        const THREADED: &str = concat!(
            r#"<ThreadedComments xmlns="http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments">"#,
            r#"<threadedComment ref="A1"><text>线程批注</text></threadedComment></ThreadedComments>"#
        );

        let dir = std::env::temp_dir().join("docparser_office_xlsx_comments");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("comments.xlsx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("xl/workbook.xml", WORKBOOK),
                ("xl/_rels/workbook.xml.rels", WB_RELS),
                ("xl/worksheets/sheet1.xml", SHEET),
                ("xl/worksheets/_rels/sheet1.xml.rels", SHEET_RELS),
                ("xl/comments1.xml", LEGACY),
                ("xl/threadedComments/threadedComment1.xml", THREADED),
            ] {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }

        let p = path.to_str().unwrap();
        let on = XlsxRawBackend::new(true).parse(p).unwrap();
        let off = XlsxRawBackend::default().parse(p).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(off.pages[0].comments.is_empty(), "默认不该读批注");
        let got: Vec<(&str, &str)> = on.pages[0]
            .comments
            .iter()
            .map(|c| (c.cell.as_str(), c.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [("A1", "线程批注")],
            "threaded 优先，legacy 不被重复计入"
        );
        // 批注不进 items（页文本口径不受开关影响）
        assert_eq!(on.pages[0].items.len(), 1);
        assert_eq!(on.pages[0].items[0].text, "甲");
    }

    /// 两 sheet 的最小工作簿（S1: ab/cd/ef，S2: zz），供预算 / 上限用例复用。
    fn two_sheet_workbook(dir: &str, file: &str) -> std::path::PathBuf {
        use std::io::Write;

        const WORKBOOK: &str = concat!(
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<sheets><sheet name="S1" sheetId="1" r:id="rId1"/>"#,
            r#"<sheet name="S2" sheetId="2" r:id="rId2"/></sheets></workbook>"#
        );
        const WB_RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>"#,
            r#"</Relationships>"#
        );
        fn sheet(rows: &[&str]) -> String {
            let body: String = rows
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    format!(
                        r#"<row r="{}"><c r="A{}" t="inlineStr"><is><t>{v}</t></is></c></row>"#,
                        i + 1,
                        i + 1
                    )
                })
                .collect();
            format!(
                concat!(
                    r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
                    "<sheetData>{body}</sheetData></worksheet>"
                ),
                body = body
            )
        }

        let dir = std::env::temp_dir().join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("xl/workbook.xml", WORKBOOK.to_string()),
                ("xl/_rels/workbook.xml.rels", WB_RELS.to_string()),
                ("xl/worksheets/sheet1.xml", sheet(&["ab", "cd", "ef"])),
                ("xl/worksheets/sheet2.xml", sheet(&["zz"])),
            ] {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        path
    }

    /// N 个 sheet 的最小工作簿，第 i 个 sheet 只有一行 `s{i}`。
    ///
    /// 专门用来验证「部件级并行」的**保序**：并行只在 >1 个 sheet 且不开预算时生效，
    /// 若切块拼接把顺序搞乱，页名与内容会一起错位。
    fn multi_sheet_workbook(dir: &str, file: &str, n: usize) -> std::path::PathBuf {
        use std::io::Write;

        let sheets: String = (1..=n)
            .map(|i| format!(r#"<sheet name="S{i}" sheetId="{i}" r:id="rId{i}"/>"#))
            .collect();
        let workbook = format!(
            concat!(
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
                r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
                "<sheets>{sheets}</sheets></workbook>"
            ),
            sheets = sheets
        );
        let rels: String = (1..=n)
            .map(|i| {
                format!(
                    concat!(
                        r#"<Relationship Id="rId{i}" "#,
                        r#"Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" "#,
                        r#"Target="worksheets/sheet{i}.xml"/>"#
                    ),
                    i = i
                )
            })
            .collect();
        let rels = format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
        );

        let dir = std::env::temp_dir().join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("xl/workbook.xml", opts).unwrap();
            zw.write_all(workbook.as_bytes()).unwrap();
            zw.start_file("xl/_rels/workbook.xml.rels", opts).unwrap();
            zw.write_all(rels.as_bytes()).unwrap();
            for i in 1..=n {
                zw.start_file(format!("xl/worksheets/sheet{i}.xml"), opts)
                    .unwrap();
                let body = format!(
                    concat!(
                        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
                        r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>s{i}</t></is></c></row>"#,
                        "</sheetData></worksheet>"
                    ),
                    i = i
                );
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        path
    }

    /// 多 sheet 并行后仍按文档序出页（页名 = workbook 顺序，内容与该 sheet 对应）。
    #[test]
    fn parallel_sheets_keep_workbook_order() {
        let path = multi_sheet_workbook("docparser_office_xlsx_parallel_order", "order.xlsx", 5);
        let p = path.to_str().unwrap().to_string();
        let doc = XlsxRawBackend::default().parse(&p).unwrap();
        std::fs::remove_file(&path).ok();

        let names: Vec<_> = doc.pages.iter().map(|p| p.name.clone().unwrap()).collect();
        assert_eq!(
            names,
            vec!["S1", "S2", "S3", "S4", "S5"],
            "并行切块必须保序"
        );
        for (i, page) in doc.pages.iter().enumerate() {
            let rows = page.items[0].table.as_ref().unwrap().rows.clone();
            assert_eq!(
                rows,
                vec![vec![format!("s{}", i + 1)]],
                "第 {} 页内容串了",
                i + 1
            );
        }
    }

    /// 最小工作簿：`sheets` 为 `<sheets>` 整段；`sheet_xml=None` 则连 sheet1.xml 一起省。
    fn minimal_xlsx(
        dir: &str,
        file: &str,
        sheets: &str,
        sheet_xml: Option<&str>,
    ) -> std::path::PathBuf {
        use std::io::Write;

        let workbook = format!(
            concat!(
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
                r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
                "{sheets}</workbook>"
            ),
            sheets = sheets
        );
        let mut parts: Vec<(&str, String)> = vec![("xl/workbook.xml", workbook)];
        if let Some(body) = sheet_xml {
            parts.push((
                "xl/_rels/workbook.xml.rels",
                concat!(
                    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
                    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
                    r#"</Relationships>"#
                )
                .to_string(),
            ));
            parts.push(("xl/worksheets/sheet1.xml", body.to_string()));
        }

        let dir = std::env::temp_dir().join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in parts {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        path
    }

    /// 零 sheet 的 workbook 是坏包：必须 `corrupt`，不能静默返回 0 页
    /// （对齐 Python `if not pages: corrupt("未解析到任何 sheet")`）。
    #[test]
    fn zero_sheet_workbook_is_corrupt() {
        let path = minimal_xlsx("docparser_office_xlsx_zero", "zero.xlsx", "<sheets/>", None);
        let err = XlsxRawBackend::default()
            .parse(path.to_str().unwrap())
            .unwrap_err();
        assert_eq!(
            err.code,
            crate::error::ErrorCode::Corrupt,
            "{}",
            err.message
        );
    }

    /// 有 sheet 但没数据 = 合法文档（1 页 0 项），不能被「零内容一律报错」误伤。
    #[test]
    fn empty_data_sheet_is_not_corrupt() {
        let path = minimal_xlsx(
            "docparser_office_xlsx_empty_sheet",
            "empty.xlsx",
            r#"<sheets><sheet name="空表" sheetId="1" r:id="rId1"/></sheets>"#,
            Some(concat!(
                r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
                r#"<sheetData/></worksheet>"#
            )),
        );
        let doc = XlsxRawBackend::default()
            .parse(path.to_str().unwrap())
            .unwrap();
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].name.as_deref(), Some("空表"));
        let joined: String = doc.pages[0].items.iter().map(|i| i.text.as_str()).collect();
        assert!(joined.is_empty(), "空表不该产出文本：{joined:?}");
    }

    /// 截断预算：按行累计字符数（行内 `,` + 行尾 `\n` 都算），整行放不下就丢该行，
    /// 命中后其余 sheet 不再解析、页级 `truncated` 置真（对齐 Python `_budget`）。
    #[test]
    fn truncate_drops_rows_and_later_sheets() {
        let path = two_sheet_workbook("docparser_office_xlsx_truncate", "truncate.xlsx");
        let p = path.to_str().unwrap().to_string();

        // 每行开销 = 2 字符 + 0 分隔符 + 1 换行 = 3；预算 5 → 只放得下第 1 行
        let on = XlsxRawBackend::default()
            .with_truncate(1, 5)
            .parse(&p)
            .unwrap();
        // threshold 命中但 size = 0 → 关闭（对齐 Python：任一为 0 即不启用）
        let off = XlsxRawBackend::default()
            .with_truncate(1, 0)
            .parse(&p)
            .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(on.pages.len(), 1, "命中预算后不再解析后续 sheet");
        let rows = on.pages[0].items[0].table.as_ref().unwrap().rows.clone();
        assert_eq!(rows, vec![vec!["ab".to_string()]]);
        assert_eq!(
            on.pages[0].items[0].meta.get("truncated"),
            Some(&crate::model::MetaValue::Bool(true))
        );
        assert_eq!(off.pages.len(), 2, "size = 0 时预算关闭");
        assert_eq!(
            off.pages[0].items[0].table.as_ref().unwrap().rows.len(),
            3,
            "预算关闭时保留全部行"
        );
    }

    /// sheet 上限：超出直接丢弃且**不**置 `truncated`（Python `sheets[:max_sheets]`），
    /// 且保留的是文档序前 N 个 sheet（页名不变）。
    #[test]
    fn max_sheets_drops_extra_sheets_without_flag() {
        let path = two_sheet_workbook("docparser_office_xlsx_max_sheets", "max.xlsx");
        let p = path.to_str().unwrap().to_string();
        let doc = XlsxRawBackend::default()
            .with_max_sheets(1)
            .parse(&p)
            .unwrap();
        let all = XlsxRawBackend::default().parse(&p).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(doc.pages.len(), 1, "只留前 1 个 sheet");
        assert_eq!(doc.pages[0].name.as_deref(), Some("S1"), "保留文档序首个");
        assert_eq!(
            doc.pages[0].items[0].meta.get("truncated"),
            Some(&crate::model::MetaValue::Bool(false)),
            "sheet 上限不是预算截断，不置 truncated"
        );
        assert_eq!(doc.pages[0].items[0].table.as_ref().unwrap().rows.len(), 3);
        assert_eq!(all.pages.len(), 2, "0 = 不限");
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    /// sheet 内嵌图（`ocr_images`）：`sheet → drawing → media`，只进 `Page.images`。
    ///
    /// 与 docx/pptx 的区别是**不去重**：Python 对每张 drawing 的每个 blip 各产一项，
    /// 同一 media 被两个 drawing 引用就出两条；去重只发生在单个 drawing 内部
    /// （`_blip_rids` 的 rid 去重）。过小图照样被 `_MIN_IMG_PX` 滤掉。
    #[test]
    fn sheet_images_read_only_when_opted_in() {
        use std::io::Write;

        const WORKBOOK: &str = concat!(
            r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<sheets><sheet name="S1" sheetId="1" r:id="rId1"/></sheets></workbook>"#
        );
        const WB_RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>"#,
            r#"</Relationships>"#
        );
        const SHEET: &str = concat!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
            r#"<sheetData><row><c t="inlineStr"><is><t>甲</t></is></c></row></sheetData></worksheet>"#
        );
        const SHEET_RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/>"#,
            r#"<Relationship Id="rId8" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing2.xml"/>"#,
            r#"</Relationships>"#
        );
        // drawing1：同一 blip 重复两次（drawing 内去重）+ 过小图（过滤）
        const DRAWING1: &str = concat!(
            r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" "#,
            r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<a:blip r:embed="rId1"/><a:blip r:embed="rId1"/><a:blip r:embed="rId3"/></xdr:wsDr>"#
        );
        // drawing2：引用同一张图 → 跨 drawing 不去重，再出一项
        const DRAWING2: &str = concat!(
            r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" "#,
            r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<a:blip r:embed="rId1"/></xdr:wsDr>"#
        );
        const DRAWING_RELS: &str = concat!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/pic1.png"/>"#,
            r#"<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/tiny.png"/>"#,
            r#"</Relationships>"#
        );

        let dir = std::env::temp_dir().join("docparser_office_xlsx_images");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("images.xlsx");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                ("xl/workbook.xml", WORKBOOK),
                ("xl/_rels/workbook.xml.rels", WB_RELS),
                ("xl/worksheets/sheet1.xml", SHEET),
                ("xl/worksheets/_rels/sheet1.xml.rels", SHEET_RELS),
                ("xl/drawings/drawing1.xml", DRAWING1),
                ("xl/drawings/drawing2.xml", DRAWING2),
                ("xl/drawings/_rels/drawing1.xml.rels", DRAWING_RELS),
                ("xl/drawings/_rels/drawing2.xml.rels", DRAWING_RELS),
            ] {
                zw.start_file(name, opts).unwrap();
                zw.write_all(body.as_bytes()).unwrap();
            }
            zw.start_file("xl/media/pic1.png", opts).unwrap();
            zw.write_all(&png_bytes(120, 80)).unwrap();
            zw.start_file("xl/media/tiny.png", opts).unwrap();
            zw.write_all(&png_bytes(20, 20)).unwrap();
            zw.finish().unwrap();
        }

        let p = path.to_str().unwrap();
        let on = XlsxRawBackend::default()
            .with_ocr_images(true)
            .parse(p)
            .unwrap();
        let off = XlsxRawBackend::default().parse(p).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(off.pages[0].images.is_empty(), "默认不该读图");
        let imgs = &on.pages[0].images;
        assert_eq!(imgs.len(), 2, "drawing1 一项 + drawing2 一项");
        for im in imgs {
            assert_eq!(im.ext, "png");
            assert_eq!(im.px, Some((120, 80)));
            assert_eq!(im.data, png_bytes(120, 80));
        }
        // 图片不进 items（sheet 文本口径不受开关影响）
        assert_eq!(on.pages[0].items.len(), 1);
        assert_eq!(on.pages[0].items[0].text, "甲");
    }
}
