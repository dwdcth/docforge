// 吸收自 firecrawl/anydoc 0.2.5（MIT，`src/formats/sheet/xlsx.rs` 共享子集），
// 见 THIRD_PARTY_NOTICES.md。
//! XLS / XLSB 共用的网格构建、数字格式化与日期序列号渲染辅助函数。

use super::controls::{cell_inlines, Checkboxes};
use super::numfmt::{builtin_code, DateParts, NumberFormat, Rendered};
use super::{format_duration_days, format_float, format_time_of_day};
use crate::backends::legacy_office::error::ConvertError;
use crate::backends::legacy_office::model::{Cell, GridBuilder, Inline, Table, TableKind};
use crate::backends::legacy_office::package::limits;
use crate::backends::legacy_office::shared::header::resolve_header_rows;
use crate::backends::legacy_office::shared::text::clean_text;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[cfg(feature = "xlsb")]
use crate::backends::legacy_office::package::path;
#[cfg(feature = "xlsb")]
use crate::backends::legacy_office::package::relationships::Relationships;

#[cfg(feature = "xlsb")]
pub(super) const SHARED_STRINGS_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings";
pub(super) const MAX_ROWS: u32 = 1_048_576;
pub(super) const MAX_COLS: u32 = 16_384;

#[cfg(feature = "xlsb")]
pub(super) fn sibling_part_name(
    rels: &Relationships,
    base: &str,
    rel: &str,
    conventional: &str,
) -> String {
    rels.first_of_type(rel)
        .and_then(|r| path::resolve(base, &r.target).ok())
        .map(|t| t.path)
        .unwrap_or_else(|| match base.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{conventional}"),
            None => conventional.to_string(),
        })
}

#[derive(Clone)]
pub(super) enum CellFormat {
    General,
    Fmt(Rc<NumberFormat>),
}

pub(super) fn resolve_format(id: u32, custom: &HashMap<u32, &str>) -> CellFormat {
    let code = custom.get(&id).copied().or_else(|| builtin_code(id));
    match code {
        Some(code) => match NumberFormat::parse(code) {
            Some(f) => CellFormat::Fmt(Rc::new(f)),
            None => {
                log::debug!("unsupported number format {code:?}, rendering as General");
                CellFormat::General
            }
        },
        None => {
            if id != 0 {
                log::debug!("numFmtId {id} has no resolvable code, rendering as General");
            }
            CellFormat::General
        }
    }
}

#[derive(Default)]
pub(super) struct SheetContent {
    pub(super) cells: HashMap<(u32, u32), String>,
    pub(super) checkboxes: Checkboxes,
    pub(super) hidden_rows: HashSet<u32>,
    pub(super) hidden_cols: Vec<(u32, u32)>,
    pub(super) merges: Vec<(u32, u32, u32, u32)>,
}

pub(super) fn render_number(fmt: &CellFormat, n: f64, date1904: bool) -> String {
    let text = match fmt {
        CellFormat::General => format_float(n),
        CellFormat::Fmt(f) => match f.format_number(n) {
            Rendered::General {
                value,
                prefix,
                suffix,
            } => format!("{prefix}{}{suffix}", format_float(value)),
            Rendered::Text(s) => s,
            Rendered::DateTime(parts) => render_serial(n, parts, date1904),
        },
    };
    clean_text(&text)
}

pub(super) fn format_as_text(fmt: &CellFormat, text: &str) -> String {
    match fmt {
        CellFormat::Fmt(f) => match f.format_text(text) {
            Some(s) => clean_text(&s),
            None => text.to_string(),
        },
        CellFormat::General => text.to_string(),
    }
}

pub(super) fn build_table(
    mut sheet: SheetContent,
    slots: &mut u64,
) -> Result<Option<Table>, ConvertError> {
    let hidden_rows = {
        let mut rows: Vec<u32> = sheet.hidden_rows.iter().copied().collect();
        rows.sort_unstable();
        rows
    };
    let hidden_cols = expand_ranges(&mut sheet.hidden_cols);
    let hidden_row = |r: u32| hidden_rows.binary_search(&r).is_ok();
    let hidden_col = |c: u32| hidden_cols.binary_search(&c).is_ok();

    let mut cells: HashMap<(u32, u32), Vec<Inline>> = HashMap::new();
    for (at, boxes) in sheet.checkboxes.drain() {
        if at.0 < MAX_ROWS && at.1 < MAX_COLS {
            cells.insert(at, cell_inlines(sheet.cells.remove(&at), &boxes));
        }
    }
    for (at, text) in sheet.cells.drain() {
        cells.insert(at, vec![Inline::plain(text)]);
    }
    sheet.merges.retain(|&(r1, c1, r2, c2)| {
        let vr = first_visible(&hidden_rows, r1, r2);
        let vc = first_visible(&hidden_cols, c1, c2);
        let (Some(vr), Some(vc)) = (vr, vc) else {
            return false;
        };
        if (vr, vc) != (r1, c1) {
            if let Some(text) = cells.remove(&(r1, c1)) {
                cells.insert((vr, vc), text);
            }
        }
        true
    });

    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for &(r, c) in cells.keys() {
        if hidden_row(r) || hidden_col(c) {
            continue;
        }
        bounds = Some(match bounds {
            None => (r, c, r, c),
            Some((r1, c1, r2, c2)) => (r1.min(r), c1.min(c), r2.max(r), c2.max(c)),
        });
    }
    let Some((mut r1, mut c1, mut r2, mut c2)) = bounds else {
        return Ok(None);
    };
    sheet
        .merges
        .retain(|&(mr1, mc1, mr2, mc2)| mr1 <= r2 && mr2 >= r1 && mc1 <= c2 && mc2 >= c1);
    for &(mr1, mc1, mr2, mc2) in &sheet.merges {
        (r1, c1, r2, c2) = (r1.min(mr1), c1.min(mc1), r2.max(mr2), c2.max(mc2));
    }

    let row_map: Vec<u32> = (r1..=r2).filter(|&r| !hidden_row(r)).collect();
    let col_map: Vec<u32> = (c1..=c2).filter(|&c| !hidden_col(c)).collect();
    if row_map.is_empty() || col_map.is_empty() {
        return Ok(None);
    }
    *slots = slots.saturating_add(row_map.len() as u64 * col_map.len() as u64);
    if *slots > limits::MAX_GRID_SLOTS {
        return Err(ConvertError::ResourceLimit {
            limit: "max_grid_slots",
            detail: format!("workbook extent covers {slots} grid positions"),
        });
    }

    let visible_span = |map: &[u32], lo: u32, hi: u32| {
        let a = map.partition_point(|&x| x < lo);
        let b = map.partition_point(|&x| x <= hi);
        (a, b - a)
    };
    let mut origins: HashMap<(usize, usize), (u32, u32)> = HashMap::new();
    let mut covered: HashSet<(usize, usize)> = HashSet::new();
    let mut expansion = 0u64;
    for &(mr1, mc1, mr2, mc2) in &sheet.merges {
        let (r0, rn) = visible_span(&row_map, mr1, mr2);
        let (c0, cn) = visible_span(&col_map, mc1, mc2);
        if rn * cn <= 1 {
            continue;
        }
        expansion = expansion.saturating_add((rn as u64) * (cn as u64) - 1);
        if expansion > limits::MAX_EXPANSION {
            return Err(ConvertError::ResourceLimit {
                limit: "max_expansion",
                detail: "merge region expansion exceeds the content budget".into(),
            });
        }
        origins.insert((r0, c0), (cn as u32, rn as u32));
        for r in r0..r0 + rn {
            for c in c0..c0 + cn {
                if (r, c) != (r0, c0) {
                    covered.insert((r, c));
                }
            }
        }
    }

    let mut builder = GridBuilder::new();
    builder.keep_covered_tail();
    for (ri, &row) in row_map.iter().enumerate() {
        builder.next_row();
        for (ci, &col) in col_map.iter().enumerate() {
            if covered.contains(&(ri, ci)) {
                builder.covered();
                continue;
            }
            let cell = match cells.remove(&(row, col)) {
                Some(inlines) => Cell::from_inlines(inlines),
                None => Cell::default(),
            };
            match origins.get(&(ri, ci)) {
                Some(&(col_span, row_span)) => {
                    builder.place(Cell::spanning(cell.blocks, col_span, row_span))?
                }
                None => builder.place(cell)?,
            }
        }
    }
    let mut table = builder.finish(TableKind::Data);
    if table.grid.is_empty() {
        return Ok(None);
    }
    table.header_rows = resolve_header_rows(&table, 0);
    Ok(Some(table))
}

fn expand_ranges(ranges: &mut [(u32, u32)]) -> Vec<u32> {
    ranges.sort_unstable();
    let mut out = Vec::new();
    let mut next = 0u32;
    for &(a, b) in ranges.iter() {
        out.extend(a.max(next)..=b);
        next = next.max(b.saturating_add(1));
    }
    out
}

fn first_visible(hidden: &[u32], lo: u32, hi: u32) -> Option<u32> {
    let tail = &hidden[hidden.partition_point(|&h| h < lo)..];
    let (mut a, mut b) = (0usize, tail.len());
    while a < b {
        let mid = (a + b) / 2;
        if tail[mid] == lo + mid as u32 {
            a = mid + 1;
        } else {
            b = mid;
        }
    }
    let first = lo.checked_add(u32::try_from(a).ok()?)?;
    (first <= hi).then_some(first)
}

fn render_serial(serial: f64, parts: DateParts, date1904: bool) -> String {
    if !serial.is_finite() {
        return format_float(serial);
    }
    if parts.elapsed {
        return format_duration_days(serial);
    }
    if !parts.date {
        return format_time_of_day(serial.fract());
    }
    if serial.abs() < 1.0 {
        return if parts.time {
            format_time_of_day(serial)
        } else {
            format_float(serial)
        };
    }
    if !(0.0..2_958_466.0).contains(&serial) {
        return format_float(serial);
    }
    let mut days = serial.trunc() as i64;
    if !date1904 && days == 60 {
        return format_float(serial);
    }
    let mut secs = (serial.fract() * 86_400.0).round() as i64;
    if secs >= 86_400 {
        secs = 0;
        days += 1;
    }
    let civil_days = if date1904 {
        days + days_from_civil(1904, 1, 1)
    } else {
        days - i64::from(days >= 60) + days_from_civil(1899, 12, 31)
    };
    let (y, m, d) = civil_from_days(civil_days);
    if !(1..=9999).contains(&y) {
        return format_float(serial);
    }
    let mut out = format!("{y:04}-{m:02}-{d:02}");
    if parts.time && secs != 0 {
        out.push_str(&format!(
            " {:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        ));
    }
    out
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (y + i64::from(m <= 2), m, d)
}
