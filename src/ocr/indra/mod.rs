//! 纯 Rust Indra OCR 引擎（feature = "ocr-indra"，基于 `fusor-ai` + 吸收自 `fusor-rs/fusor-ai` 0.1.5 MIT）。
//!
//! 特点：
//! - **100% 纯 Rust、零 C/C++ 编译依赖**：支持 ±45° 纠偏、90°/180°/竖排朝向检测、
//!   80+ 语种（含 CJK 中文）CTC + n-gram 识别、**独立数学公式转 LaTeX** 与阅读顺序版面重建；
//! - **自动下载 + 首次本地编译缓存**：下载 `datalab-to/indra-ocr` 固定修订（19 个文件，SHA-256 严格校验），
//!   中国大陆 IP 自动走 `hf-mirror.com` / ModelScope 加速，境外直连 HuggingFace；
//!   首次运行在进程内耗时约 1 秒编译为 `fusor` bundle 并持久化缓存，后续加载 < 0.3 秒；
//! - **模型许可提示**：源码为 MIT，Datalab Indra OCR 模型权重遵循 modified OpenRAIL-M（运行时由用户按需下载，不随二进制分发）。

#[allow(dead_code, unused_imports, clippy::all)]
pub(crate) mod compiler;
#[allow(dead_code, unused_imports, clippy::all)]
pub(crate) mod engine;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bundle::{Engine as BundleEngine, Manifest, Store};
use media::image::{Pixels, Size};

use crate::ocr::download::{self, DownloadSpec, MirrorPolicy};
use crate::ocr::{OcrEngine, RecognizedLine};

const INDRA_REV: &str = "bd02167fdf2005a19962cc15188f8496a2c87d3b";

/// `datalab-to/indra-ocr` 固定修订全部 19 个校验文件（与 `indra-ocr-wasm/scripts/fetch-checkpoint.sh` 一致）。
const CHECKPOINT_FILES: &[(&str, &str, u64)] = &[
    ("LICENSE", "50c88a018a7ee322e89f6b09401b237beb0cd331685879405b3db48993950a5f", 12_000),
    ("config.json", "8d23884924b5ee9f6a1eb33e477b6b367d1a6da2d2a007fab7323008b4c189a8", 512),
    ("meta.json", "4ce4c94b46f5d644033c02e718322014ff4081270f385038c571912b56696339", 64_000),
    ("math_meta.json", "85d9715fb8a77791b55ef89f57fb41e72eb3170ec58a7c3bee3b394268bf2b10", 8_000),
    ("det.onnx", "f2bd4cba23e9c328cf7ec824634e423e191a2c6aacbbf35c62bd79dd70f62f59", 2_500_000),
    ("orient.onnx", "20eaf670f55a7cf9facfe1bdd6c7b8e40be75bbf4d48a3df43f66aa3bbd874fd", 1_500_000),
    ("script.onnx", "b6ce4e926e19c243faaf37bd46880398bf2cb4ae56a29e1177a3b017c1280e36", 1_000_000),
    ("rec.onnx", "475972a2710be4a3cbfeb2bc7f3624ffd563de07de42476fc7a290d4e1ece484", 5_000_000),
    ("rec_cjk.onnx", "4a035516a17e62cdf33a4fe2d88077654cb8841cff024685cdef4e351694ef50", 9_000_000),
    ("rec_rtl.onnx", "468796bd684486a8388c30c8b4ed2a26e3eeb1f8ff02831c24c2e12c79a2a834", 4_000_000),
    ("rec_world.onnx", "c8d1e59f21b04966f179d2d2d8e22b230d22110b3f89855bbbf2624a58c40043", 6_000_000),
    ("order.onnx", "a42db2e8b536ad3d84045a4b7f9b0a72667cf8be4aa02d63f7b38004719fb180", 3_000_000),
    ("mathcls.onnx", "818b1556e9ef337958db6be0c9791299d3f74a4575586b0cd469aaae9534ff96", 1_000_000),
    ("math_enc.onnx", "d4e4a6c40dad1ee988d32c6403b1590ca42864d4483bbe1e671c5dbb999a5a97", 6_000_000),
    ("math_dec_init.onnx", "919417fde9962c3318aeb478c1fe3955dd2bb8cd34476c68debed2ba39e5dcd6", 5_000_000),
    ("math_dec_step.onnx", "5ff6e6cbaf0dea70314f66222a1469736a8c5e7994fab06a5b4e53c90469239f", 5_000_000),
    ("lm.npz", "6c0fb618a363a0acdd05d4f5b0edd9e8146d2d990541b87f6130bbfe17cea58d", 15_000_000),
    ("lm_cjk.npz", "6d64f13f4f90f4e529bd63340c6df76238d007feaef2abb903d8cd28d92bc2f3", 20_000_000),
    ("lm_world.npz", "34df5e74a2fcbb9cf5e7f360a559375ffa2e04d2d2943a42308c177bcb444d5d", 10_000_000),
];

/// Indra OCR 引擎配置。
#[derive(Debug, Clone)]
pub struct IndraConfig {
    /// 用户显式指定的离线/自定义目录（支持已编译 bundle 目录或原始 checkpoint 目录）。
    pub model_dir: Option<PathBuf>,
    /// 镜像选择策略（默认 `Auto` = IP 归属地自动探测）。
    pub mirror: MirrorPolicy,
    /// 是否启用数学公式 → LaTeX 识别（默认 `true`）。
    pub math: bool,
}

impl Default for IndraConfig {
    fn default() -> Self {
        Self {
            model_dir: None,
            mirror: MirrorPolicy::Auto,
            math: true,
        }
    }
}

/// Indra OCR 引擎实例（懒加载，线程安全）。
pub struct IndraOcrEngine {
    cfg: IndraConfig,
    inner: Mutex<Option<engine::session::Engine>>,
}

impl IndraOcrEngine {
    pub fn new(cfg: IndraConfig) -> Self {
        Self {
            cfg,
            inner: Mutex::new(None),
        }
    }

    pub fn shared(cfg: IndraConfig) -> Arc<dyn OcrEngine> {
        Arc::new(Self::new(cfg))
    }

    fn ensure_loaded(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Option<engine::session::Engine>>, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "Indra OCR 引擎锁中毒".to_string())?;
        if guard.is_none() {
            let bundle_dir = prepare_bundle(&self.cfg)?;
            let loaded = load_bundle(&bundle_dir)?;
            *guard = Some(loaded);
        }
        Ok(guard)
    }

    /// 识别整页并返回完整的 Indra `Page`（含版面块与结构化 Markdown）。
    pub fn recognize_page(&self, image: &image::RgbaImage) -> Result<engine::assemble::Page, String> {
        let mut guard = self.ensure_loaded()?;
        let eng = guard.as_mut().expect("ensure_loaded 已初始化");
        let (w, h) = image.dimensions();
        let pixels = Pixels {
            size: Size {
                width: w as usize,
                height: h as usize,
            },
            channels: 4,
            values: image.as_raw().clone(),
        };
        let opts = engine::session::Options {
            math: self.cfg.math,
        };
        eng.start(&pixels, opts);
        loop {
            let progress = eng
                .step()
                .map_err(|e| format!("Indra OCR 推理失败：{e}"))?;
            if let Some(page) = progress.page {
                return Ok(page);
            }
        }
    }
}

impl OcrEngine for IndraOcrEngine {
    fn name(&self) -> &str {
        "fusor-ai indra-ocr"
    }

    fn recognize(&self, image: &image::RgbaImage) -> Result<Vec<RecognizedLine>, String> {
        let page = self.recognize_page(image)?;
        Ok(page
            .lines
            .into_iter()
            .filter(|l| !l.text.trim().is_empty())
            .map(|l| {
                let [x0, y0, x1, y1] = [
                    l.bbox[0] as f32,
                    l.bbox[1] as f32,
                    l.bbox[2] as f32,
                    l.bbox[3] as f32,
                ];
                RecognizedLine {
                    text: l.text,
                    score: l.conf as f32,
                    quad: [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
                }
            })
            .collect())
    }
}

fn prepare_bundle(cfg: &IndraConfig) -> Result<PathBuf, String> {
    let base_root = match &cfg.model_dir {
        Some(dir) => {
            // 1. 直接给的就是已编译的 bundle 目录（含 manifest.json）
            if dir.join("manifest.json").is_file() {
                return Ok(dir.clone());
            }
            // 2. 给的目录下已有 compiled bundle 子目录
            let sub_bundle = dir.join("bundle");
            if sub_bundle.join("manifest.json").is_file() {
                return Ok(sub_bundle);
            }
            // 3. 给的是离线 raw checkpoint 目录（含 det.onnx & config.json）：直接就地编译到 bundle/
            if dir.join("det.onnx").is_file() && dir.join("config.json").is_file() {
                return compile_checkpoint_to_bundle(dir, &sub_bundle);
            }
            dir.clone()
        }
        None => download::default_models_root().join("indra-ocr"),
    };

    let bundle_dir = base_root.join("bundle");
    if bundle_dir.join("manifest.json").is_file() {
        return Ok(bundle_dir);
    }

    let ckpt_dir = base_root.join("checkpoint");
    download_checkpoint(&ckpt_dir, cfg.mirror)?;
    compile_checkpoint_to_bundle(&ckpt_dir, &bundle_dir)
}

fn download_checkpoint(ckpt_dir: &Path, mirror: MirrorPolicy) -> Result<(), String> {
    fs::create_dir_all(ckpt_dir)
        .map_err(|e| format!("创建 Indra checkpoint 目录失败（{}）：{e}", ckpt_dir.display()))?;

    for &(file, sha, size_hint) in CHECKPOINT_FILES {
        let hf_mirror = format!("https://hf-mirror.com/datalab-to/indra-ocr/resolve/{INDRA_REV}/{file}");
        let modelscope = format!(
            "https://www.modelscope.cn/api/v1/models/datalab-to/indra-ocr/repo?Revision=master&FilePath={file}"
        );
        let hf_official = format!("https://huggingface.co/datalab-to/indra-ocr/resolve/{INDRA_REV}/{file}");

        let cn_urls = [hf_mirror.as_str(), modelscope.as_str()];
        let global_urls = [hf_official.as_str(), hf_mirror.as_str()];
        let spec = DownloadSpec {
            name: file,
            sha256: Some(sha),
            size_hint,
            cn_urls: &cn_urls,
            global_urls: &global_urls,
        };
        download::ensure_file(&ckpt_dir.join(file), &spec, mirror)?;
    }
    Ok(())
}

fn compile_checkpoint_to_bundle(ckpt_dir: &Path, bundle_dir: &Path) -> Result<PathBuf, String> {
    let tmp_bundle = bundle_dir.with_file_name(format!(".bundle.{}.tmp", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_bundle);
    eprintln!(
        "[ocr] 正在将 Indra OCR checkpoint 编译为本地加速 bundle（仅首次耗时 ~1s）..."
    );
    compiler::run(ckpt_dir, &tmp_bundle, compiler::Profile::F32).map_err(|e| {
        let _ = fs::remove_dir_all(&tmp_bundle);
        format!("编译 Indra OCR bundle 失败：{e}")
    })?;
    let license_src = ckpt_dir.join("LICENSE");
    if license_src.is_file() {
        let _ = fs::copy(&license_src, tmp_bundle.join("LICENSE"));
    }
    if bundle_dir.exists() {
        let _ = fs::remove_dir_all(bundle_dir);
    }
    fs::rename(&tmp_bundle, bundle_dir).map_err(|e| {
        let _ = fs::remove_dir_all(&tmp_bundle);
        format!("落盘 Indra bundle 目录失败（{}）：{e}", bundle_dir.display())
    })?;
    Ok(bundle_dir.to_path_buf())
}

fn load_bundle(dir: &Path) -> Result<engine::session::Engine, String> {
    const CHUNK: usize = 8 << 20;
    let manifest_path = dir.join("manifest.json");
    let manifest_json = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("读取 Indra manifest 失败（{}）：{e}", manifest_path.display()))?;
    let manifest =
        Manifest::parse(&manifest_json).map_err(|e| format!("解析 Indra manifest 失败：{e}"))?;
    let mut store =
        Store::new(manifest.clone()).map_err(|e| format!("初始化 Indra Store 失败：{e}"))?;
    for (index, part) in manifest.parts.iter().enumerate() {
        let part_path = dir.join(&part.file);
        let bytes = fs::read(&part_path)
            .map_err(|e| format!("读取 Indra 模型分片失败（{}）：{e}", part_path.display()))?;
        for chunk in bytes.chunks(CHUNK) {
            store
                .write(index, chunk)
                .map_err(|e| format!("写入 Indra 模型分片失败：{e}"))?;
        }
        store
            .end_part(index)
            .map_err(|e| format!("校验 Indra 模型分片失败：{e}"))?;
    }
    engine::session::Engine::new(&mut store)
        .map_err(|e| format!("构建 Indra OCR 引擎失败：{e}"))
}
