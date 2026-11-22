//! 纯 Rust PP-OCRv6 引擎（feature = "ocr-ppocr"，基于 `qppocr` 0.5）。
//!
//! 特点：
//! - **100% 纯 Rust、零 C/C++ 编译依赖**：手写 AVX2（x86_64）/ NEON（aarch64）/ scalar（`loongarch64` 等）内核，
//!   直读官方 PP-OCRv6 ONNX 原件，不依赖 ONNX Runtime；
//! - **懒加载 + 自动镜像下载**：首次 `recognize()` 触发模型准备（纯数字文档零网络、零内存开销）；
//!   中国大陆 IP 自动走 ModelScope（`greatv/oar-ocr`）加速，境外直连 HuggingFace（`PaddlePaddle/PP-OCRv6_*_onnx`），
//!   均带 SHA-256 严格校验；
//! - **离线优先**：支持 `--ocr-model-dir` 指定本地目录（兼容 `{tier}/{det,rec}.onnx` 与平铺 `pp-ocrv6_{tier}_{det,rec}.onnx` 两种布局）。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::ocr::download::{self, DownloadSpec, MirrorPolicy, Region};
use crate::ocr::{OcrEngine, RecognizedLine};

/// PP-OCRv6 模型档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PpOcrTier {
    /// 极轻（~6.3 MB：det 1.8 MB + rec 4.5 MB，精简 6,904 字表）。
    Tiny,
    /// 默认均衡（~31 MB：det 9.9 MB + rec 21.2 MB，全量 18,708 字表，中英日混排稳健）。
    #[default]
    Small,
    /// 高精度（~139 MB：det 62 MB + rec 76.6 MB，全量 18,708 字表）。
    Medium,
}

impl PpOcrTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Small => "small",
            Self::Medium => "medium",
        }
    }

    fn to_qppocr(self) -> qppocr::Tier {
        match self {
            Self::Tiny => qppocr::Tier::Tiny,
            Self::Small => qppocr::Tier::Small,
            Self::Medium => qppocr::Tier::Medium,
        }
    }
}

/// PP-OCRv6 引擎配置。
#[derive(Debug, Clone, Default)]
pub struct PpOcrConfig {
    pub tier: PpOcrTier,
    /// 用户显式指定的离线/自定义模型目录；`None` 时走默认缓存目录并按需自动下载。
    pub model_dir: Option<PathBuf>,
    /// 镜像选择策略（默认 `Auto` = IP 归属地自动探测）。
    pub mirror: MirrorPolicy,
}

/// PP-OCRv6 引擎实例（懒加载，线程安全）。
pub struct PpOcrEngine {
    cfg: PpOcrConfig,
    inner: Mutex<Option<qppocr::Engine>>,
}

impl PpOcrEngine {
    pub fn new(cfg: PpOcrConfig) -> Self {
        Self {
            cfg,
            inner: Mutex::new(None),
        }
    }

    pub fn shared(cfg: PpOcrConfig) -> Arc<dyn OcrEngine> {
        Arc::new(Self::new(cfg))
    }

    fn ensure_loaded(&self) -> Result<std::sync::MutexGuard<'_, Option<qppocr::Engine>>, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "PP-OCRv6 引擎锁中毒".to_string())?;
        if guard.is_none() {
            let root = prepare_models(&self.cfg)?;
            let engine = qppocr::Engine::builder()
                .tier(self.cfg.tier.to_qppocr())
                .detect_orientation(false)
                .build(&root)
                .map_err(|e| format!("加载 PP-OCRv6 模型失败（{}）：{e}", root.display()))?;
            *guard = Some(engine);
        }
        Ok(guard)
    }
}

impl OcrEngine for PpOcrEngine {
    fn name(&self) -> &str {
        match self.cfg.tier {
            PpOcrTier::Tiny => "qppocr pp-ocrv6-tiny",
            PpOcrTier::Small => "qppocr pp-ocrv6-small",
            PpOcrTier::Medium => "qppocr pp-ocrv6-medium",
        }
    }

    fn recognize(&self, image: &image::RgbaImage) -> Result<Vec<RecognizedLine>, String> {
        let guard = self.ensure_loaded()?;
        let engine = guard.as_ref().expect("ensure_loaded 已初始化");

        let (w, h) = image.dimensions();
        let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
        for px in image.pixels() {
            rgb.push(px[0]);
            rgb.push(px[1]);
            rgb.push(px[2]);
        }
        let img = qppocr::rgb_from_bytes(w, h, rgb)
            .map_err(|e| format!("构造 PP-OCRv6 输入图像失败：{e}"))?;
        let result = engine
            .run(&img)
            .map_err(|e| format!("PP-OCRv6 推理失败：{e}"))?;

        Ok(result
            .lines
            .into_iter()
            .filter(|l| !l.text.trim().is_empty())
            .map(|l| RecognizedLine {
                text: l.text,
                score: l.confidence,
                quad: l.pts,
            })
            .collect())
    }
}

struct TierSpecs {
    det: DownloadSpec<'static>,
    rec: DownloadSpec<'static>,
    dict_ms: DownloadSpec<'static>,
    hf_yml_urls: &'static [&'static str],
    flat_det: &'static str,
    flat_rec: &'static str,
    flat_dict: &'static str,
}

fn tier_specs(tier: PpOcrTier) -> TierSpecs {
    match tier {
        PpOcrTier::Tiny => TierSpecs {
            det: DownloadSpec {
                name: "pp-ocrv6_tiny_det.onnx",
                sha256: Some("193bab7a04fca699a6c82e6abb5b81bdb28177f0abd4062552b04908dafb19f8"),
                size_hint: 1_780_590,
                cn_urls: &[
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_tiny_det.onnx",
                    "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_tiny_det_onnx/resolve/main/inference.onnx",
                ],
                global_urls: &[
                    "https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_det_onnx/resolve/main/inference.onnx",
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_tiny_det.onnx",
                ],
            },
            rec: DownloadSpec {
                name: "pp-ocrv6_tiny_rec.onnx",
                sha256: Some("9ef676d6ed3c88256a2d92c640c44f25b0c40947e111b14b8be8f594091563e6"),
                size_hint: 4_462_639,
                cn_urls: &[
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_tiny_rec.onnx",
                    "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_tiny_rec_onnx/resolve/main/inference.onnx",
                ],
                global_urls: &[
                    "https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_rec_onnx/resolve/main/inference.onnx",
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_tiny_rec.onnx",
                ],
            },
            dict_ms: DownloadSpec {
                name: "ppocrv6_tiny_dict.txt",
                sha256: Some("c5cbe34ef40c29c4df07ed012bf96569cb69a2d2a01a07027e9f13cb832bd9cd"),
                size_hint: 27_156,
                cn_urls: &["https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=ppocrv6_tiny_dict.txt"],
                global_urls: &["https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=ppocrv6_tiny_dict.txt"],
            },
            hf_yml_urls: &[
                "https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_rec_onnx/resolve/main/inference.yml",
                "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_tiny_rec_onnx/resolve/main/inference.yml",
            ],
            flat_det: "pp-ocrv6_tiny_det.onnx",
            flat_rec: "pp-ocrv6_tiny_rec.onnx",
            flat_dict: "ppocrv6_tiny_dict.txt",
        },
        PpOcrTier::Small => TierSpecs {
            det: DownloadSpec {
                name: "pp-ocrv6_small_det.onnx",
                sha256: Some("d73e0058b7a8086bbd57f3d10b8bcd4ff95363f67e06e2762b5e814fe9c9410e"),
                size_hint: 9_880_512,
                cn_urls: &[
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_small_det.onnx",
                    "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_small_det_onnx/resolve/main/inference.onnx",
                ],
                global_urls: &[
                    "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det_onnx/resolve/main/inference.onnx",
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_small_det.onnx",
                ],
            },
            rec: DownloadSpec {
                name: "pp-ocrv6_small_rec.onnx",
                sha256: Some("5435fd747c9e0efe15a96d0b378d5bd157e9492ed8fd80edf08f30d02fa24634"),
                size_hint: 21_159_378,
                cn_urls: &[
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_small_rec.onnx",
                    "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_small_rec_onnx/resolve/main/inference.onnx",
                ],
                global_urls: &[
                    "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec_onnx/resolve/main/inference.onnx",
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_small_rec.onnx",
                ],
            },
            dict_ms: DownloadSpec {
                name: "ppocrv6_dict.txt",
                sha256: Some("b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d"),
                size_hint: 74_947,
                cn_urls: &["https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=ppocrv6_dict.txt"],
                global_urls: &["https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=ppocrv6_dict.txt"],
            },
            hf_yml_urls: &[
                "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec_onnx/resolve/main/inference.yml",
                "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_small_rec_onnx/resolve/main/inference.yml",
            ],
            flat_det: "pp-ocrv6_small_det.onnx",
            flat_rec: "pp-ocrv6_small_rec.onnx",
            flat_dict: "ppocrv6_dict.txt",
        },
        PpOcrTier::Medium => TierSpecs {
            det: DownloadSpec {
                name: "pp-ocrv6_medium_det.onnx",
                sha256: Some("eb13b44b25bb36f89528b68720af8a61d9cf381176107f465db1757b65d086e1"),
                size_hint: 62_032_837,
                cn_urls: &[
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_medium_det.onnx",
                    "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_medium_det_onnx/resolve/main/inference.onnx",
                ],
                global_urls: &[
                    "https://huggingface.co/PaddlePaddle/PP-OCRv6_medium_det_onnx/resolve/main/inference.onnx",
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_medium_det.onnx",
                ],
            },
            rec: DownloadSpec {
                name: "pp-ocrv6_medium_rec.onnx",
                sha256: Some("9c09abf0957f7968c7586464b7397b84ad2387a0497a351af40e9acc71b673ba"),
                size_hint: 76_554_979,
                cn_urls: &[
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_medium_rec.onnx",
                    "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_medium_rec_onnx/resolve/main/inference.onnx",
                ],
                global_urls: &[
                    "https://huggingface.co/PaddlePaddle/PP-OCRv6_medium_rec_onnx/resolve/main/inference.onnx",
                    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=pp-ocrv6_medium_rec.onnx",
                ],
            },
            dict_ms: DownloadSpec {
                name: "ppocrv6_dict.txt",
                sha256: Some("b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d"),
                size_hint: 74_947,
                cn_urls: &["https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=ppocrv6_dict.txt"],
                global_urls: &["https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=ppocrv6_dict.txt"],
            },
            hf_yml_urls: &[
                "https://huggingface.co/PaddlePaddle/PP-OCRv6_medium_rec_onnx/resolve/main/inference.yml",
                "https://hf-mirror.com/PaddlePaddle/PP-OCRv6_medium_rec_onnx/resolve/main/inference.yml",
            ],
            flat_det: "pp-ocrv6_medium_det.onnx",
            flat_rec: "pp-ocrv6_medium_rec.onnx",
            flat_dict: "ppocrv6_dict.txt",
        },
    }
}

/// 准备 PP-OCRv6 模型目录（符合 `qppocr::ModelSource::Dir` 布局 `{root}/{tier}/{det,rec,dict}`）。
fn prepare_models(cfg: &PpOcrConfig) -> Result<PathBuf, String> {
    let specs = tier_specs(cfg.tier);

    // 1. 用户指定了模型目录：优先离线直接使用（兼容 qppocr 布局与 oar-ocr 平铺命名）
    if let Some(user_dir) = &cfg.model_dir {
        if let Some(ready) = detect_existing_layout(user_dir, cfg.tier, &specs)? {
            return Ok(ready);
        }
        // 目录存在但缺文件：若目录为空/缺件，下载到该目录（让 --ocr-model-dir 既可离线又可自定义缓存位）
        return download_into_root(user_dir, cfg.tier, &specs, cfg.mirror);
    }

    // 2. 默认缓存目录：<cache>/ppocrv6
    let root = download::default_models_root().join("ppocrv6");
    download_into_root(&root, cfg.tier, &specs, cfg.mirror)
}

fn detect_existing_layout(
    user_dir: &Path,
    tier: PpOcrTier,
    specs: &TierSpecs,
) -> Result<Option<PathBuf>, String> {
    let tier_dir = user_dir.join(tier.as_str());
    let has_std = tier_dir.join("det.onnx").is_file()
        && tier_dir.join("rec.onnx").is_file()
        && (tier_dir.join("dict.txt").is_file()
            || user_dir.join("dict.txt").is_file()
            || user_dir.join("ppocr_keys.txt").is_file());
    if has_std {
        return Ok(Some(user_dir.to_path_buf()));
    }

    // 兼容平铺目录（如 ~/.oar 里的 pp-ocrv6_small_det.onnx + pp-ocrv6_small_rec.onnx + ppocrv6_dict.txt）
    let flat_det = user_dir.join(specs.flat_det);
    let flat_rec = user_dir.join(specs.flat_rec);
    let flat_dict = user_dir.join(specs.flat_dict);
    if flat_det.is_file() && flat_rec.is_file() && flat_dict.is_file() {
        fs::create_dir_all(&tier_dir)
            .map_err(|e| format!("创建模型布局子目录失败（{}）：{e}", tier_dir.display()))?;
        link_or_copy(&flat_det, &tier_dir.join("det.onnx"))?;
        link_or_copy(&flat_rec, &tier_dir.join("rec.onnx"))?;
        link_or_copy(&flat_dict, &tier_dir.join("dict.txt"))?;
        return Ok(Some(user_dir.to_path_buf()));
    }
    Ok(None)
}

fn link_or_copy(src: &Path, dst: &Path) -> Result<(), String> {
    if dst.is_file() {
        return Ok(());
    }
    if fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst).map_err(|e| {
        format!(
            "复制模型文件失败（{} → {}）：{e}",
            src.display(),
            dst.display()
        )
    })?;
    Ok(())
}

fn download_into_root(
    root: &Path,
    tier: PpOcrTier,
    specs: &TierSpecs,
    mirror: MirrorPolicy,
) -> Result<PathBuf, String> {
    let tier_dir = root.join(tier.as_str());
    let det_path = tier_dir.join("det.onnx");
    let rec_path = tier_dir.join("rec.onnx");
    let dict_path = tier_dir.join("dict.txt");

    download::ensure_file(&det_path, &specs.det, mirror)?;
    download::ensure_file(&rec_path, &specs.rec, mirror)?;

    if !dict_path.is_file() || fs::metadata(&dict_path).map(|m| m.len()).unwrap_or(0) == 0 {
        let region = mirror.resolve();
        match region {
            Region::China => {
                // 中国优先 ModelScope 直取纯文本字典，失败则从 HF inference.yml 提取
                if download::ensure_file(&dict_path, &specs.dict_ms, MirrorPolicy::China).is_err() {
                    let yml = download::download_text_from_urls(specs.hf_yml_urls)?;
                    let dict = extract_dict_from_inference_yml(&yml)
                        .ok_or_else(|| "无法从 inference.yml 提取 PP-OCRv6 字符字典".to_string())?;
                    fs::write(&dict_path, dict)
                        .map_err(|e| format!("写出字典失败（{}）：{e}", dict_path.display()))?;
                }
            }
            Region::Global => {
                // 境外优先从 HuggingFace inference.yml 提取，失败回退 ModelScope
                if let Ok(yml) = download::download_text_from_urls(specs.hf_yml_urls) {
                    if let Some(dict) = extract_dict_from_inference_yml(&yml) {
                        fs::write(&dict_path, dict)
                            .map_err(|e| format!("写出字典失败（{}）：{e}", dict_path.display()))?;
                        return Ok(root.to_path_buf());
                    }
                }
                download::ensure_file(&dict_path, &specs.dict_ms, MirrorPolicy::Global)?;
            }
        }
    }

    Ok(root.to_path_buf())
}

/// 从 PaddleOCR 官方 `inference.yml` 的 `character_dict:` 节解析逐行字典文本。
///
/// 注意：不可对整行做 `.trim()`，否则会把全角空格 `\u{3000}`（如 `  - 　`）误裁剪成 `-` 导致提前截断。
pub(crate) fn extract_dict_from_inference_yml(yml: &str) -> Option<String> {
    let mut lines = Vec::new();
    let mut in_dict = false;
    let mut base_indent: Option<usize> = None;

    for raw in yml.lines() {
        let raw = raw.trim_end_matches('\r');
        let indent = raw.bytes().take_while(|&b| b == b' ').count();
        let rest_after_indent = &raw[indent..];
        if !in_dict {
            if rest_after_indent.trim() == "character_dict:" {
                in_dict = true;
            }
            continue;
        }
        if rest_after_indent.is_empty() || rest_after_indent.starts_with('#') {
            continue;
        }
        if let Some(bi) = base_indent {
            if indent < bi && !rest_after_indent.starts_with("- ") {
                break;
            }
        }
        let Some(item) = rest_after_indent.strip_prefix("- ") else {
            if base_indent.is_some() {
                break;
            }
            continue;
        };
        if base_indent.is_none() {
            base_indent = Some(indent);
        }
        lines.push(parse_yaml_scalar_item(item));
    }

    if lines.len() < 100 {
        return None;
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Some(out)
}

fn parse_yaml_scalar_item(s: &str) -> String {
    // 仅去掉纯 YAML 语义无关的 \r，保留全角空格 \u{3000} 等字典字符本身
    let t = s.trim_end_matches('\r');
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return t[1..t.len() - 1].replace("''", "'");
    }
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let inner = &t[1..t.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('x') => {
                        let hex: String = chars.by_ref().take(2).collect();
                        if let Ok(v) = u32::from_str_radix(&hex, 16) {
                            if let Some(ch) = char::from_u32(v) {
                                out.push(ch);
                            }
                        }
                    }
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).collect();
                        if let Ok(v) = u32::from_str_radix(&hex, 16) {
                            if let Some(ch) = char::from_u32(v) {
                                out.push(ch);
                            }
                        }
                    }
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_character_dict_from_inference_yml() {
        let mut yml = String::from("PostProcess:\n  name: CTCLabelDecode\n  character_dict:\n");
        yml.push_str("  - '!'\n  - '\"'\n  - ''''\n  - \"\\\\\"\n  - \u{3000}\n");
        for i in 0..120 {
            yml.push_str(&format!("  - c{i}\n"));
        }
        yml.push_str("OtherSection:\n  foo: bar\n");
        let dict = extract_dict_from_inference_yml(&yml).unwrap();
        let rows: Vec<&str> = dict.lines().collect();
        assert_eq!(rows[0], "!");
        assert_eq!(rows[1], "\"");
        assert_eq!(rows[2], "'");
        assert_eq!(rows[3], "\\");
        assert_eq!(rows[4], "\u{3000}");
        assert_eq!(rows.len(), 125);
    }
}
