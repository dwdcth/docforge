//! Open Knowledge Format（OKF v0.3）输出模块。
//!
//! 对齐 Google 开源的 [Open Knowledge Format v0.3](https://github.com/open-knowledge-format/open-knowledge-format)：
//! - 单份文档输出为合法的 **Concept**（YAML 1.2 frontmatter + CommonMark/GFM 正文），
//!   必填字段 `type`，自带 `generated`（`docforge/<version>`）、`sources`、`status`、
//!   以及 `x-docforge-*` 扩展溯源键；
//! - 内嵌图片提取为 **Asset**（按内容去重、嗅探真实扩展名、受 [`crate::limits::MAX_ASSET_TOTAL_BYTES`]
//!   约束），以相对路径 `assets/...` 写入 Markdown 链接；
//! - 批量目录输出为完整 **Knowledge Bundle**：自动规避保留文件名（`index.md` / `log.md`），
//!   根目录生成带 `okf_version: "0.3"` frontmatter 的 `index.md`，子目录生成无 frontmatter 的
//!   `index.md`（渐进式披露，严格遵循 OKF §3.1 / §8）。
//!
//! 零第三方依赖（不引入 `serde_yaml` 或时间库）。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::emit::to_gfm_with_assets;
use crate::limits::MAX_ASSET_TOTAL_BYTES;
use crate::model::{BlockKind, BlockSource, Document};

/// OKF 规范版本号（SPEC §8.1 要求引号包裹的字符串）。
pub const OKF_VERSION: &str = "0.3";

/// 单个 Concept 的输出选项。
#[derive(Debug, Clone)]
pub struct OkfOptions {
    /// OKF 必填字段 `type`（默认 `"Document"`）。
    pub concept_type: String,
    /// 可选覆盖标题；为 `None` 时依次取 `meta.extra["title"]` / 首个 Heading / 源文件名。
    pub title: Option<String>,
    /// 可选覆盖一句话摘要；为 `None` 时取首段正文压缩成单行。
    pub description: Option<String>,
    /// 源文档标识（路径/文件名/URI），写入 `resource` 与 `sources[0].resource`。
    pub resource: Option<String>,
    /// 资产文件名安全前缀（如源文件名 stem），防止同目录多文档资产重名。
    pub asset_prefix: Option<String>,
    /// 是否提取并生成 `assets/` 图片资产（stdout 模式可关闭）。
    pub include_assets: bool,
    /// 可选覆盖 `generated.at`（RFC 3339 UTC；为 `None` 时取当前 UTC 时间）。
    pub generated_at: Option<String>,
    /// 可选源文件修改时间（RFC 3339 UTC，写入 `sources[0].last_modified`）。
    pub source_last_modified: Option<String>,
    /// 附加标签。
    pub extra_tags: Vec<String>,
}

impl Default for OkfOptions {
    fn default() -> Self {
        Self {
            concept_type: "Document".to_string(),
            title: None,
            description: None,
            resource: None,
            asset_prefix: None,
            include_assets: true,
            generated_at: None,
            source_last_modified: None,
            extra_tags: Vec::new(),
        }
    }
}

/// OKF 资产文件（非 `.md`，如内嵌图片）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OkfAsset {
    /// 相对所在 Concept 目录的 POSIX 路径（如 `"assets/report-p1-img1.png"`）。
    pub rel_path: String,
    /// 原始图片字节。
    pub data: Vec<u8>,
}

/// 单份文档渲染为 OKF Concept 的完整产物（Markdown + 资产列表 + 索引元数据）。
#[derive(Debug, Clone)]
pub struct OkfConcept {
    pub markdown: String,
    pub concept_type: String,
    pub title: String,
    pub description: String,
    pub assets: Vec<OkfAsset>,
}

/// Bundle 索引条目（供 `render_root_index` / `render_subdir_index` 生成逐级 `index.md`）。
#[derive(Debug, Clone)]
pub struct OkfIndexEntry {
    /// 相对 Bundle 根目录的 Concept 文件路径（POSIX，含 `.md`，如 `"sub/report.md"`）。
    pub rel_path: String,
    pub title: String,
    pub description: String,
    pub concept_type: String,
}

/// 把 `Document` 渲染为 OKF v0.3 Concept（YAML 1.2 frontmatter + GFM 正文 + 可选图片资产）。
pub fn to_okf_concept(doc: &Document, opts: &OkfOptions) -> OkfConcept {
    let concept_type = if opts.concept_type.trim().is_empty() {
        "Document".to_string()
    } else {
        opts.concept_type.trim().to_string()
    };

    let title = opts
        .title
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| derive_title(doc))
        .or_else(|| {
            opts.resource
                .as_deref()
                .and_then(|r| Path::new(r).file_stem())
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Untitled Document".to_string());

    let description = opts
        .description
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| derive_description(doc));

    // 图片资产提取（按 dedup_key / 内容哈希去重，受 MAX_ASSET_TOTAL_BYTES 硬上限保护）
    let (assets, per_page_links) = if opts.include_assets {
        extract_assets(doc, opts.asset_prefix.as_deref().unwrap_or("doc"))
    } else {
        (Vec::new(), Vec::new())
    };

    let body = to_gfm_with_assets(doc, &per_page_links);

    // 统计 OCR / 信任信号
    let mut ocr_pages = 0usize;
    let mut low_conf_ocr = false;
    for page in &doc.pages {
        let mut page_has_ocr = false;
        for b in &page.blocks {
            if b.source == BlockSource::Ocr {
                page_has_ocr = true;
                if b.confidence.is_some_and(|c| c < 0.8) {
                    low_conf_ocr = true;
                }
            }
        }
        if page_has_ocr {
            ocr_pages += 1;
        }
    }
    let has_warnings = doc.meta.extra.contains_key("warnings");
    let status = if low_conf_ocr || has_warnings {
        "draft"
    } else {
        "stable"
    };

    // 标签去重汇总
    let mut tags: Vec<String> = Vec::new();
    let mut seen_tags: BTreeSet<String> = BTreeSet::new();
    let mut push_tag = |t: &str| {
        let clean = t.trim();
        if !clean.is_empty() && seen_tags.insert(clean.to_ascii_lowercase()) {
            tags.push(clean.to_string());
        }
    };
    if !doc.meta.file_type.is_empty() {
        push_tag(&doc.meta.file_type);
    }
    if let Some(lang) = doc.meta.extra.get("language") {
        push_tag(lang);
    }
    for t in &opts.extra_tags {
        push_tag(t);
    }

    let generated_at = opts
        .generated_at
        .clone()
        .unwrap_or_else(|| format_rfc3339_utc(SystemTime::now()));

    // 拼装严格符合 YAML 1.2 核心模式的 frontmatter
    let mut fm = String::new();
    fm.push_str("---\n");
    fm.push_str(&format!("type: {}\n", yaml_str(&concept_type)));
    fm.push_str(&format!("title: {}\n", yaml_str(&title)));
    if !description.is_empty() {
        fm.push_str(&format!("description: {}\n", yaml_str(&description)));
    }
    if let Some(ref res) = opts.resource {
        if !res.trim().is_empty() {
            fm.push_str(&format!("resource: {}\n", yaml_str(res.trim())));
        }
    }
    if !tags.is_empty() {
        let items: Vec<String> = tags.iter().map(|t| yaml_str(t)).collect();
        fm.push_str(&format!("tags: [{}]\n", items.join(", ")));
    }
    fm.push_str(&format!("status: {status}\n"));
    fm.push_str(&format!(
        "generated: {{ by: \"docforge/{}\", at: {} }}\n",
        env!("CARGO_PKG_VERSION"),
        yaml_str(&generated_at)
    ));

    if let Some(ref res) = opts.resource {
        if !res.trim().is_empty() {
            fm.push_str("sources:\n");
            fm.push_str("  - id: source-document\n");
            fm.push_str(&format!("    resource: {}\n", yaml_str(res.trim())));
            fm.push_str(&format!("    title: {}\n", yaml_str(&title)));
            if let Some(author) = doc
                .meta
                .extra
                .get("authors")
                .or_else(|| doc.meta.extra.get("author"))
                .filter(|a| !a.trim().is_empty())
            {
                fm.push_str(&format!(
                    "    author: {}\n",
                    yaml_str(&format!("human:{}", author.trim()))
                ));
            }
            if let Some(ref lm) = opts.source_last_modified {
                fm.push_str(&format!("    last_modified: {}\n", yaml_str(lm)));
            }
        }
    }

    // OKF §4.1 扩展键：统一用 `x-docforge-*` 前缀
    if !doc.meta.file_type.is_empty() {
        fm.push_str(&format!(
            "x-docforge-format: {}\n",
            yaml_str(&doc.meta.file_type)
        ));
    }
    fm.push_str(&format!("x-docforge-pages: {}\n", doc.pages.len()));
    if ocr_pages > 0 {
        fm.push_str(&format!("x-docforge-ocr-pages: {ocr_pages}\n"));
    }
    if !assets.is_empty() {
        fm.push_str(&format!("x-docforge-assets: {}\n", assets.len()));
    }
    fm.push_str("---\n\n");

    let mut markdown = fm;
    markdown.push_str(&body);
    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }

    OkfConcept {
        markdown,
        concept_type,
        title,
        description,
        assets,
    }
}

/// 规避 OKF §3.1 保留文件名（`index` / `log` 不允许作为 Concept 文件名）。
pub fn safe_concept_stem(stem: &str) -> String {
    if stem.eq_ignore_ascii_case("index") || stem.eq_ignore_ascii_case("log") {
        format!("{stem}-doc")
    } else {
        stem.to_string()
    }
}

/// 为 OKF Bundle 的每一层目录生成 `index.md` 映射：`相对目录路径 -> index.md 内容`。
///
/// 严格遵守 OKF SPEC §8：
/// - 根目录（`PathBuf::new()`）带 frontmatter（含 `okf_version: "0.3"`）；
/// - 子目录**不带 frontmatter**；
/// - 链接目标按 §6 对空格等保留字符做 percent-encoding。
pub fn build_bundle_indexes(
    bundle_title: &str,
    bundle_description: &str,
    entries: &[OkfIndexEntry],
) -> BTreeMap<PathBuf, String> {
    // 目录 → (直接子目录集合, 直接子 Concept 列表)
    let mut dirs: BTreeMap<PathBuf, (BTreeSet<String>, Vec<&OkfIndexEntry>)> = BTreeMap::new();
    dirs.entry(PathBuf::new()).or_default();

    for entry in entries {
        let p = Path::new(&entry.rel_path);
        let parent = p.parent().unwrap_or(Path::new("")).to_path_buf();
        dirs.entry(parent.clone()).or_default().1.push(entry);

        // 向上补齐祖先目录的直接子目录链接
        let mut cur = parent;
        while let Some(name) = cur.file_name().map(|n| n.to_string_lossy().into_owned()) {
            let grand = cur.parent().unwrap_or(Path::new("")).to_path_buf();
            dirs.entry(grand.clone()).or_default().0.insert(name);
            cur = grand;
        }
    }

    let mut out = BTreeMap::new();
    for (dir, (subdirs, mut concepts)) in dirs {
        concepts.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        let mut md = String::new();
        if dir.as_os_str().is_empty() {
            // 根目录 index.md：带 §8.1 Bundle 元数据 frontmatter
            md.push_str("---\n");
            md.push_str(&format!("okf_version: \"{OKF_VERSION}\"\n"));
            md.push_str(&format!("title: {}\n", yaml_str(bundle_title)));
            if !bundle_description.trim().is_empty() {
                md.push_str(&format!(
                    "description: {}\n",
                    yaml_str(bundle_description.trim())
                ));
            }
            md.push_str("---\n\n");
        }

        if !subdirs.is_empty() {
            md.push_str("# Subdirectories\n\n");
            for sub in &subdirs {
                let href = format!("{}/", encode_uri_component(sub));
                md.push_str(&format!("* [{sub}]({href}) - Subdirectory\n"));
            }
            if !concepts.is_empty() {
                md.push('\n');
            }
        }

        if !concepts.is_empty() {
            md.push_str("# Documents\n\n");
            for c in concepts {
                let file_name = Path::new(&c.rel_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| c.rel_path.clone());
                let href = encode_uri_component(&file_name);
                if c.description.is_empty() {
                    md.push_str(&format!("* [{}]({href})\n", c.title));
                } else {
                    md.push_str(&format!("* [{}]({href}) - {}\n", c.title, c.description));
                }
            }
        }

        out.insert(dir.join("index.md"), md);
    }
    out
}

/// 格式化 `SystemTime` 为 RFC 3339 UTC 字符串（`"YYYY-MM-DDTHH:MM:SSZ"`），零外部依赖。
pub fn format_rfc3339_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Howard Hinnant 算法：自 1970-01-01 起的整数天数 → 公历 `(year, month, day)`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = y + i64::from(m <= 2);
    (year, m, d)
}

fn derive_title(doc: &Document) -> Option<String> {
    for key in ["title", "epub.title", "fb2.title", "mobi.title", "chm.title"] {
        if let Some(v) = doc.meta.extra.get(key) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    doc.blocks()
        .find(|b| b.kind == BlockKind::Heading && !b.content.trim().is_empty())
        .map(|b| collapse_ws(&b.content, 120))
}

fn derive_description(doc: &Document) -> String {
    for b in doc.blocks() {
        if matches!(b.kind, BlockKind::Paragraph | BlockKind::Quote) {
            let clean = collapse_ws(&b.content, 160);
            if clean.chars().count() >= 4 {
                return clean;
            }
        }
    }
    String::new()
}

fn collapse_ws(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut prev_ws = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                if count >= max_chars {
                    out.push('…');
                    break;
                }
                out.push(' ');
                count += 1;
                prev_ws = true;
            }
        } else {
            if count >= max_chars {
                out.push('…');
                break;
            }
            out.push(c);
            count += 1;
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

fn extract_assets(doc: &Document, raw_prefix: &str) -> (Vec<OkfAsset>, Vec<Vec<String>>) {
    let prefix = sanitize_filename(raw_prefix);
    let mut assets: Vec<OkfAsset> = Vec::new();
    let mut per_page_links: Vec<Vec<String>> = Vec::with_capacity(doc.pages.len());
    let mut dedup: HashMap<String, String> = HashMap::new();
    let mut total_bytes = 0usize;

    for (p_idx, page) in doc.pages.iter().enumerate() {
        let mut links = Vec::new();
        for (im_idx, im) in page.images.iter().enumerate() {
            if im.data.is_empty() {
                continue;
            }
            let key = im
                .dedup_key
                .clone()
                .unwrap_or_else(|| format!("fnv:{:016x}:{}", fnv1a64(&im.data), im.data.len()));
            if let Some(existing_path) = dedup.get(&key) {
                links.push(existing_path.clone());
                continue;
            }
            if total_bytes.saturating_add(im.data.len()) > MAX_ASSET_TOTAL_BYTES {
                continue;
            }
            let ext = sniff_image_ext(&im.data);
            let rel_path = format!("assets/{prefix}-p{}-img{}.{ext}", p_idx + 1, im_idx + 1);
            total_bytes += im.data.len();
            dedup.insert(key, rel_path.clone());
            assets.push(OkfAsset {
                rel_path: rel_path.clone(),
                data: im.data.clone(),
            });
            links.push(rel_path);
        }
        per_page_links.push(links);
    }

    (assets, per_page_links)
}

fn sniff_image_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif"
    } else if bytes.starts_with(b"BM") {
        "bmp"
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        "tif"
    } else if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
        "svg"
    } else {
        "bin"
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn sanitize_filename(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
            out.push(c);
        } else if (c.is_whitespace() || c == '.') && !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "doc".to_string()
    } else {
        trimmed.chars().take(48).collect()
    }
}

/// YAML 1.2 双引号安全编码（避免数字/布尔/冒号/换行等歧义，严格符合 OKF §4）。
fn yaml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// OKF §6 URI 路径编码：编码空格与保留字符，保留安全文件名字符及 UTF-8 字符的百分号编码。
pub(crate) fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, DocMeta, Image, Page};

    #[test]
    fn okf_concept_emits_spec_v03_frontmatter_and_deduped_assets() {
        let png = b"\x89PNG\r\n\x1a\nFAKEPNG".to_vec();
        let doc = Document {
            meta: DocMeta {
                file_type: "docx".into(),
                extra: [("language".into(), "cmn".into())].into_iter().collect(),
                ..Default::default()
            },
            pages: vec![Page {
                blocks: vec![
                    Block {
                        kind: BlockKind::Heading,
                        level: Some(1),
                        content: "系统架构说明".into(),
                        ..Default::default()
                    },
                    Block {
                        kind: BlockKind::Paragraph,
                        content: "这是用于验证 OKF 概念文档输出的正文段落。".into(),
                        ..Default::default()
                    },
                    Block {
                        kind: BlockKind::Image,
                        content: "图内文字".into(),
                        meta: [("alt".into(), "架构图".into())].into_iter().collect(),
                        ..Default::default()
                    },
                ],
                images: vec![
                    Image {
                        data: png.clone(),
                        ..Default::default()
                    },
                    // 重复图片应去重为同一资产文件
                    Image {
                        data: png,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
        }
        .finalize();

        let opts = OkfOptions {
            resource: Some("arch.docx".into()),
            asset_prefix: Some("arch".into()),
            generated_at: Some("2026-11-22T08:00:00Z".into()),
            ..Default::default()
        };
        let concept = to_okf_concept(&doc, &opts);
        assert_eq!(concept.assets.len(), 1, "同字节图片应去重");
        assert_eq!(concept.assets[0].rel_path, "assets/arch-p1-img1.png");
        assert!(concept.markdown.starts_with("---\n"));
        assert!(concept.markdown.contains("type: \"Document\"\n"));
        assert!(concept.markdown.contains("title: \"系统架构说明\"\n"));
        assert!(concept.markdown.contains("status: stable\n"));
        assert!(concept
            .markdown
            .contains("generated: { by: \"docforge/"));
        assert!(concept
            .markdown
            .contains("![架构图](assets/arch-p1-img1.png)"));
    }

    #[test]
    fn okf_bundle_indexes_root_has_frontmatter_and_subdir_has_none() {
        let entries = vec![
            OkfIndexEntry {
                rel_path: "overview.md".into(),
                title: "Overview".into(),
                description: "Top level doc".into(),
                concept_type: "Document".into(),
            },
            OkfIndexEntry {
                rel_path: "sub/my report.md".into(),
                title: "My Report".into(),
                description: "Nested doc".into(),
                concept_type: "Document".into(),
            },
        ];
        let indexes = build_bundle_indexes("Demo Bundle", "Converted by docforge.", &entries);
        let root = indexes.get(Path::new("index.md")).expect("root index");
        assert!(root.starts_with("---\nokf_version: \"0.3\"\n"));
        assert!(root.contains("* [sub](sub/)"));
        assert!(root.contains("* [Overview](overview.md) - Top level doc"));

        let sub = indexes.get(Path::new("sub/index.md")).expect("subdir index");
        assert!(
            !sub.starts_with("---"),
            "OKF §8: 子目录 index.md 不得含 frontmatter: {sub}"
        );
        assert!(sub.contains("* [My Report](my%20report.md) - Nested doc"));
    }

    #[test]
    fn reserved_concept_names_are_renamed() {
        assert_eq!(safe_concept_stem("index"), "index-doc");
        assert_eq!(safe_concept_stem("INDEX"), "INDEX-doc");
        assert_eq!(safe_concept_stem("log"), "log-doc");
        assert_eq!(safe_concept_stem("report"), "report");
    }

    #[test]
    fn rfc3339_epoch_and_known_date() {
        assert_eq!(format_rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        // 2026-01-01T00:00:00Z = 1767225600
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_767_225_600);
        assert_eq!(format_rfc3339_utc(t), "2026-01-01T00:00:00Z");
    }
}
