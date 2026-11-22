//! 纯 Rust OCR 模型自动下载器（feature = "ocr-download"）。
//!
//! 特性：
//! - **100% 纯 Rust HTTPS**：`ureq` + `rustls` + `rustls-rustcrypto` + `webpki-roots`，
//!   不依赖 OpenSSL / `ring` / `aws-lc-rs`，全 7 平台（含 `loongarch64` / `linux-musl`）无痛交叉编译；
//! - **IP 归属地自动探测**：按需联网探测本机公网 IP 归属地（中国大陆 vs 境外），
//!   中国大陆自动走 ModelScope / `hf-mirror.com` 镜像加速，境外直连 HuggingFace，
//!   同时支持 `--ocr-mirror auto|cn|global` 与 `DOCFORGE_OCR_MIRROR` 显式覆盖；
//! - **完整校验与防残片**：流式 SHA-256 校验 + `.sha256` sidecar 免重复哈希 +
//!   `.part` 原子临时文件 RAII 清理 + 跨镜像自动回退。

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// 缓存目录环境变量覆盖名。
pub const MODEL_DIR_ENV: &str = "DOCFORGE_OCR_MODEL_DIR";
/// 镜像策略环境变量覆盖名（`auto` | `cn` | `global`）。
pub const MIRROR_ENV: &str = "DOCFORGE_OCR_MIRROR";

/// 检测到的网络区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// 中国大陆：优先 ModelScope / `hf-mirror.com`。
    China,
    /// 境外：优先 HuggingFace 官方源。
    Global,
}

/// 镜像选择策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MirrorPolicy {
    /// 自动探测公网 IP 归属地（默认；若设置 `DOCFORGE_OCR_MIRROR` 则优先遵从环境变量）。
    #[default]
    Auto,
    /// 强制中国镜像加速（ModelScope / `hf-mirror.com` 优先）。
    China,
    /// 强制境外官方源（HuggingFace 优先）。
    Global,
}

impl MirrorPolicy {
    /// 解析 CLI/环境变量取值（`auto` | `cn` | `china` | `global` | `overseas`）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "cn" | "china" | "mirror" => Some(Self::China),
            "global" | "overseas" | "hf" => Some(Self::Global),
            _ => None,
        }
    }

    /// 按策略结合环境变量与 IP 探测给出最终区域。
    pub fn resolve(self) -> Region {
        match self {
            Self::China => Region::China,
            Self::Global => Region::Global,
            Self::Auto => {
                if let Ok(env_val) = std::env::var(MIRROR_ENV) {
                    if let Some(p) = Self::parse(&env_val) {
                        match p {
                            Self::China => return Region::China,
                            Self::Global => return Region::Global,
                            Self::Auto => {}
                        }
                    }
                }
                detect_region()
            }
        }
    }
}

/// 单个待下载模型文件的描述。
#[derive(Debug, Clone)]
pub struct DownloadSpec<'a> {
    /// 展示名 / 相对路径（如 `small/det.onnx`）。
    pub name: &'a str,
    /// 期望的小写 64 位 SHA-256；`None` 表示跳过哈希校验（如转换生成文件）。
    pub sha256: Option<&'a str>,
    /// 粗略大小（仅用于进度提示，不做硬性拒绝）。
    pub size_hint: u64,
    /// 中国大陆优先 URL 列表（按顺序尝试）。
    pub cn_urls: &'a [&'a str],
    /// 境外优先 URL 列表（按顺序尝试）。
    pub global_urls: &'a [&'a str],
}

/// 构造纯 Rust TLS 的 `ureq::Agent`（零 `ring` / `aws-lc-rs` / OpenSSL）。
pub fn build_http_agent(timeout_secs: u64) -> ureq::Agent {
    let provider = Arc::new(rustls_rustcrypto::provider());
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(provider)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_connect(Some(Duration::from_secs(15.min(timeout_secs))))
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .user_agent(concat!("docforge/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

/// 解析默认模型缓存根目录。
///
/// 优先级：`DOCFORGE_OCR_MODEL_DIR` → 平台缓存目录（`$XDG_CACHE_HOME/docforge/models` /
/// `~/Library/Caches/docforge/models` / `%LOCALAPPDATA%/docforge/models`）→ `~/.cache/docforge/models`。
pub fn default_models_root() -> PathBuf {
    if let Some(env_dir) = std::env::var_os(MODEL_DIR_ENV) {
        let p = PathBuf::from(env_dir);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            if !local.is_empty() {
                return PathBuf::from(local).join("docforge").join("models");
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            return home.join("Library").join("Caches").join("docforge").join("models");
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("docforge").join("models");
        }
    }
    if let Some(home) = home_dir() {
        return home.join(".cache").join("docforge").join("models");
    }
    PathBuf::from(".docforge-models")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// 探测当前公网 IP 位于中国大陆还是境外（进程内 `OnceLock` 缓存，仅在首次实际下载模型时触发）。
pub fn detect_region() -> Region {
    static CACHED: OnceLock<Region> = OnceLock::new();
    *CACHED.get_or_init(probe_region_uncached)
}

fn probe_region_uncached() -> Region {
    let agent = build_http_agent(4);

    // 1. 全球极简 GeoIP（返回 {"ip":"...","country":"CN"}）
    if let Ok(body) = http_get_text(&agent, "https://api.country.is/") {
        if let Some(cc) = parse_country_is_json(&body) {
            let region = if cc == "CN" { Region::China } else { Region::Global };
            eprintln!("[ocr] IP 归属地探测: {cc} → {}", region_label(region));
            return region;
        }
    }

    // 2. ipinfo.io 纯文本国家码（返回 "CN\n" / "US\n" 等）
    if let Ok(body) = http_get_text(&agent, "https://ipinfo.io/country") {
        if let Some(cc) = parse_plain_country_code(&body) {
            let region = if cc == "CN" { Region::China } else { Region::Global };
            eprintln!("[ocr] IP 归属地探测: {cc} → {}", region_label(region));
            return region;
        }
    }

    // 3. 国内直连兜底（境外接口在受限内网超时时，测太平洋 IP 接口；若返回国内省市代码则为 CN）
    if let Ok(body) = http_get_text(&agent, "https://whois.pconline.com.cn/ipJson.jsp?json=true") {
        if looks_like_china_pconline(&body) {
            eprintln!("[ocr] IP 归属地探测: CN（国内兜底接口）→ {}", region_label(Region::China));
            return Region::China;
        }
    }

    // 4. 连通性兜底：若 HuggingFace 不可达而 ModelScope 可达，则切国内镜像
    let hf_ok = agent.head("https://huggingface.co").call().is_ok();
    if !hf_ok && agent.head("https://www.modelscope.cn").call().is_ok() {
        eprintln!("[ocr] 网络探测: HuggingFace 不可达、ModelScope 可达 → {}", region_label(Region::China));
        return Region::China;
    }

    eprintln!("[ocr] IP 归属地探测未命中，默认使用 {}", region_label(Region::Global));
    Region::Global
}

fn region_label(r: Region) -> &'static str {
    match r {
        Region::China => "中国大陆镜像加速（ModelScope / hf-mirror）",
        Region::Global => "境外官方源（HuggingFace）",
    }
}

fn http_get_text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let resp = agent.get(url).call().map_err(|e| e.to_string())?;
    let mut reader = resp.into_body().into_reader();
    let mut buf = Vec::new();
    reader
        .by_ref()
        .take(4096)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 解析 `https://api.country.is/` 返回的 `{"ip":"...","country":"CN"}`。
fn parse_country_is_json(body: &str) -> Option<String> {
    let key = "\"country\"";
    let idx = body.find(key)?;
    let rest = &body[idx + key.len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let quoted = after.strip_prefix('"')?;
    let end = quoted.find('"')?;
    parse_plain_country_code(&quoted[..end])
}

fn parse_plain_country_code(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() == 2 && t.bytes().all(|b| b.is_ascii_alphabetic()) {
        Some(t.to_ascii_uppercase())
    } else {
        None
    }
}

fn looks_like_china_pconline(body: &str) -> bool {
    // 太平洋接口国内 IP 含非空 "proCode":"110000" 等 6 位行政区划码，境外返回 "proCode":"999999" 或空
    if let Some(idx) = body.find("\"proCode\"") {
        let rest = &body[idx + 9..];
        if let Some(q1) = rest.find('"') {
            let after = &rest[q1 + 1..];
            if let Some(q2) = after.find('"') {
                let code = &after[..q2];
                return code.len() == 6
                    && code.bytes().all(|b| b.is_ascii_digit())
                    && code != "999999"
                    && code != "000000";
            }
        }
    }
    false
}

/// 构造候选 URL 序列（主区域优先，次区域自动兜底去重）。
pub fn ordered_urls<'a>(spec: &DownloadSpec<'a>, region: Region) -> Vec<&'a str> {
    let (primary, secondary) = match region {
        Region::China => (spec.cn_urls, spec.global_urls),
        Region::Global => (spec.global_urls, spec.cn_urls),
    };
    let mut out = Vec::with_capacity(primary.len() + secondary.len());
    for &u in primary.iter().chain(secondary.iter()) {
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

/// 确保目标文件存在于磁盘且通过 SHA-256 校验；不存在或校验失败时自动按镜像策略下载。
pub fn ensure_file(
    target: &Path,
    spec: &DownloadSpec<'_>,
    mirror: MirrorPolicy,
) -> Result<PathBuf, String> {
    if file_matches_cache(target, spec.sha256) {
        return Ok(target.to_path_buf());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("创建模型目录失败（{}）：{e}", parent.display()))?;
    }

    let region = mirror.resolve();
    let urls = ordered_urls(spec, region);
    if urls.is_empty() {
        return Err(format!("模型 `{}` 未配置任何下载地址", spec.name));
    }

    let agent = build_http_agent(30 * 60);
    let mut errors: Vec<String> = Vec::new();
    for url in urls {
        for attempt in 1..=2 {
            let size_label = if spec.size_hint >= 1024 * 1024 {
                format!("{:.1} MiB", spec.size_hint as f64 / (1024.0 * 1024.0))
            } else if spec.size_hint >= 1024 {
                format!("{:.1} KiB", spec.size_hint as f64 / 1024.0)
            } else {
                format!("{} B", spec.size_hint)
            };
            eprintln!(
                "[ocr] 下载 {}（{}，尝试 {attempt}/2）← {}",
                spec.name,
                size_label,
                short_host(url)
            );
            match download_once(&agent, url, target, spec.sha256) {
                Ok(()) => return Ok(target.to_path_buf()),
                Err(e) => {
                    eprintln!("[ocr] 下载失败（{}）：{e}", short_host(url));
                    errors.push(format!("{url}: {e}"));
                }
            }
        }
    }

    Err(format!(
        "模型 `{}` 所有镜像均下载失败（可改用 --ocr-model-dir 指定离线模型目录）：\n  {}",
        spec.name,
        errors.join("\n  ")
    ))
}

/// 直接下载文本内容（如 HuggingFace `inference.yml`），按镜像顺序自动回退。
pub fn download_text_from_urls(urls: &[&str]) -> Result<String, String> {
    let agent = build_http_agent(120);
    let mut errors = Vec::new();
    for &url in urls {
        match agent.get(url).call() {
            Ok(resp) => {
                let mut text = String::new();
                resp.into_body()
                    .into_reader()
                    .read_to_string(&mut text)
                    .map_err(|e| format!("读取响应失败（{url}）：{e}"))?;
                return Ok(text);
            }
            Err(e) => errors.push(format!("{url}: {e}")),
        }
    }
    Err(format!("下载文本失败：{}", errors.join("; ")))
}

fn short_host(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.split('/').next().unwrap_or(rest)
}

fn file_matches_cache(path: &Path, expected_sha256: Option<&str>) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() == 0 {
        return false;
    }
    let Some(want) = expected_sha256 else {
        return true;
    };
    if sidecar_matches(path, want) {
        return true;
    }
    match sha256_file(path) {
        Ok(got) if got.eq_ignore_ascii_case(want) => {
            let _ = write_sidecar(path, want);
            true
        }
        _ => false,
    }
}

fn sidecar_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    Some(path.with_file_name(format!(".{name}.sha256")))
}

fn sidecar_matches(path: &Path, expected: &str) -> bool {
    sidecar_path(path)
        .and_then(|p| fs::read_to_string(p).ok())
        .is_some_and(|s| s.trim().eq_ignore_ascii_case(expected))
}

fn write_sidecar(path: &Path, hash: &str) -> io::Result<()> {
    if let Some(p) = sidecar_path(path) {
        fs::write(p, hash)?;
    }
    Ok(())
}

/// 计算文件的 SHA-256 小写十六进制摘要。
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempGuard(Option<PathBuf>);

impl TempGuard {
    fn new(p: PathBuf) -> Self {
        Self(Some(p))
    }
    fn path(&self) -> &Path {
        self.0.as_deref().expect("temp path active")
    }
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = fs::remove_file(p);
        }
    }
}

fn download_once(
    agent: &ureq::Agent,
    url: &str,
    target: &Path,
    expected_sha256: Option<&str>,
) -> Result<(), String> {
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("HTTP 请求失败：{e}"))?;

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let fname = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model".to_string());
    let tmp = target.with_file_name(format!(".{fname}.{}.{seq}.part", std::process::id()));
    let guard = TempGuard::new(tmp);

    let mut out = File::create(guard.path())
        .map_err(|e| format!("创建临时文件失败（{}）：{e}", guard.path().display()))?;
    let mut reader = resp.into_body().into_reader();
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;

    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("下载读取中断：{e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| format!("写入临时文件失败：{e}"))?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    out.flush().map_err(|e| format!("刷盘失败：{e}"))?;
    drop(out);

    if total == 0 {
        return Err("服务端返回空文件".to_string());
    }

    let actual_hash = hex_lower(&hasher.finalize());
    if let Some(want) = expected_sha256 {
        if !actual_hash.eq_ignore_ascii_case(want) {
            return Err(format!(
                "SHA-256 校验不匹配（期望 {want}，实际 {actual_hash}）"
            ));
        }
    }

    // Windows 上 rename 覆盖已有目标前需先删旧文件
    if target.exists() {
        let _ = fs::remove_file(target);
    }
    fs::rename(guard.path(), target)
        .map_err(|e| format!("落盘重命名失败（{}）：{e}", target.display()))?;
    guard.disarm();
    if let Some(want) = expected_sha256 {
        let _ = write_sidecar(target, want);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_geoip_responses() {
        assert_eq!(
            parse_country_is_json(r#"{"ip":"1.2.3.4","country":"CN"}"#).as_deref(),
            Some("CN")
        );
        assert_eq!(
            parse_country_is_json(r#"{"ip":"8.8.8.8","country":"US"}"#).as_deref(),
            Some("US")
        );
        assert_eq!(parse_plain_country_code("  jp\n").as_deref(), Some("JP"));
        assert_eq!(parse_plain_country_code("invalid"), None);
        assert!(looks_like_china_pconline(r#"{"ip":"1.1.1.1","proCode":"110000"}"#));
        assert!(!looks_like_china_pconline(r#"{"ip":"8.8.8.8","proCode":"999999"}"#));
    }

    #[test]
    fn mirror_policy_orders_urls_by_region() {
        let spec = DownloadSpec {
            name: "test.onnx",
            sha256: None,
            size_hint: 10,
            cn_urls: &["https://cn.example/a", "https://shared.example/a"],
            global_urls: &["https://global.example/a", "https://shared.example/a"],
        };
        assert_eq!(
            ordered_urls(&spec, Region::China),
            vec![
                "https://cn.example/a",
                "https://shared.example/a",
                "https://global.example/a"
            ]
        );
        assert_eq!(
            ordered_urls(&spec, Region::Global),
            vec![
                "https://global.example/a",
                "https://shared.example/a",
                "https://cn.example/a"
            ]
        );
    }

    #[test]
    fn cached_file_sha256_and_sidecar_work() {
        let dir = std::env::temp_dir().join(format!("docforge_dl_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("sample.bin");
        fs::write(&f, b"hello docforge").unwrap();
        let h = sha256_file(&f).unwrap();
        assert!(file_matches_cache(&f, Some(&h)));
        assert!(sidecar_matches(&f, &h));
        assert!(!file_matches_cache(&f, Some("0000000000000000000000000000000000000000000000000000000000000000")));
        let _ = fs::remove_dir_all(&dir);
    }
}
