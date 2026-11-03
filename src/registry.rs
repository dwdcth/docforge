// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! 注册表：能力 / 后端注册、按策略解析后端链、复合后端委托环检测。

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::plugin::{FormatPlugin, ParseCapability, ProcessCapability};
use crate::policy::BackendPolicy;

/// 复合后端不可委托成环。
#[derive(Debug)]
pub struct RegistryError(pub String);

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "registry: {}", self.0)
    }
}

impl std::error::Error for RegistryError {}

#[derive(Default)]
pub struct Registry {
    parse_caps: HashMap<String, Vec<Arc<dyn ParseCapability>>>,
    process_caps: HashMap<String, Arc<dyn ProcessCapability>>,
    plugins: Vec<Arc<dyn FormatPlugin>>,
    delegates: HashMap<String, String>, // backend id -> delegate id
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册解析后端；同一声明的多个 format 共享同一 `Arc`。
    pub fn register_parse(
        &mut self,
        cap: Arc<dyn ParseCapability>,
        delegate_to: Option<&str>,
    ) -> Result<(), RegistryError> {
        if let Some(dst) = delegate_to {
            self.check_delegate_cycle(cap.id(), dst)?;
            self.delegates.insert(cap.id().to_string(), dst.to_string());
        }
        for f in cap.formats() {
            self.parse_caps
                .entry((*f).to_string())
                .or_default()
                .push(cap.clone());
        }
        Ok(())
    }

    pub fn register_process(&mut self, cap: Arc<dyn ProcessCapability>) {
        self.process_caps.insert(cap.id().to_string(), cap);
    }

    pub fn register_plugin(&mut self, plugin: Arc<dyn FormatPlugin>) {
        self.plugins.push(plugin);
    }

    /// 按格式找插件。
    pub fn plugin_for(&self, fmt: &str) -> Option<Arc<dyn FormatPlugin>> {
        self.plugins
            .iter()
            .find(|p| p.formats().contains(&fmt))
            .cloned()
    }

    fn check_delegate_cycle(&self, src: &str, dst: &str) -> Result<(), RegistryError> {
        if src == dst {
            return Err(RegistryError(format!(
                "backend cannot delegate to itself: {src}"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        let mut cur = Some(dst.to_string());
        while let Some(c) = cur {
            if c == src {
                return Err(RegistryError(format!(
                    "delegate cycle: {src} -> ... -> {dst}"
                )));
            }
            if !seen.insert(c.clone()) {
                break;
            }
            cur = self.delegates.get(&c).cloned();
        }
        Ok(())
    }

    /// 解析该格式可尝试的解析后端链（过滤不可用项）。
    pub fn resolve_parse_chain(
        &self,
        fmt: &str,
        policy: &BackendPolicy,
        default: &[&str],
    ) -> Vec<Arc<dyn ParseCapability>> {
        let chain =
            policy.ordered_chain(&default.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let caps = self.parse_caps.get(fmt).cloned().unwrap_or_default();
        let picked: Vec<Arc<dyn ParseCapability>> = chain
            .iter()
            .filter_map(|bid| {
                caps.iter()
                    .find(|c| c.id() == bid)
                    .filter(|c| c.is_available())
                    .cloned()
            })
            .collect();
        if !picked.is_empty() {
            return picked;
        }
        // 空链兜底：一个插件可以声明多条后端链（如字幕 srt/vtt/ass 共用一个插件），
        // 而 `allow_fallback = false` 会把链截断到 1 项——截掉的正好是"属于该格式"的后端时，
        // 上面就一个都匹配不上（表现为「全部解析后端失败」，且是空链、没有任何尝试记录）。
        // 此时退回"注册在该格式下的后端"（注册序）：空链永远是错误结果，退回只会把
        // "必然失败"变成"按默认顺序尝试"，不会推翻调用方显式指定的可用后端。
        caps.into_iter().filter(|c| c.is_available()).collect()
    }

    pub fn resolve_process_chain(
        &self,
        policy: &BackendPolicy,
        default: &[&str],
    ) -> Vec<Arc<dyn ProcessCapability>> {
        let chain =
            policy.ordered_chain(&default.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        chain
            .iter()
            .filter_map(|bid| self.process_caps.get(bid.as_str()).cloned())
            .filter(|c| c.is_available())
            .filter(|c| c.id() != "ocr" || policy.enable_ocr)
            .collect()
    }

    pub fn list_backends(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .parse_caps
            .values()
            .flatten()
            .map(|c| format!("{} ({})", c.id(), c.kind()))
            .chain(self.process_caps.keys().cloned())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::plugin::RawDoc;

    struct MockParse {
        id: &'static str,
        formats: Vec<&'static str>,
        available: bool,
    }
    impl ParseCapability for MockParse {
        fn id(&self) -> &str {
            self.id
        }
        fn formats(&self) -> &[&str] {
            &self.formats
        }
        fn is_available(&self) -> bool {
            self.available
        }
        fn parse(&self, _path: &str) -> Result<RawDoc> {
            Ok(RawDoc::default())
        }
    }

    struct MockProc {
        id: &'static str,
    }
    impl ProcessCapability for MockProc {
        fn id(&self) -> &str {
            self.id
        }
        fn process(&self, _raw: &mut RawDoc) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn register_and_resolve_multi_format() {
        let mut reg = Registry::new();
        let cap: Arc<dyn ParseCapability> = Arc::new(MockParse {
            id: "fmt",
            formats: vec!["a", "b"],
            available: true,
        });
        reg.register_parse(cap, None).unwrap();
        let pol = BackendPolicy::default();
        assert_eq!(reg.resolve_parse_chain("a", &pol, &["fmt"]).len(), 1);
        assert_eq!(reg.resolve_parse_chain("b", &pol, &["fmt"]).len(), 1);
        assert_eq!(reg.resolve_parse_chain("c", &pol, &["fmt"]).len(), 0);
    }

    #[test]
    fn unavailable_is_skipped() {
        let mut reg = Registry::new();
        reg.register_parse(
            Arc::new(MockParse {
                id: "x",
                formats: vec!["a"],
                available: false,
            }),
            None,
        )
        .unwrap();
        let pol = BackendPolicy::default();
        assert_eq!(reg.resolve_parse_chain("a", &pol, &["x"]).len(), 0);
    }

    #[test]
    fn process_chain_respects_enable_ocr() {
        let mut reg = Registry::new();
        reg.register_process(Arc::new(MockProc { id: "ocr" }));
        let on = BackendPolicy {
            enable_ocr: true,
            ..Default::default()
        };
        let off = BackendPolicy {
            enable_ocr: false,
            ..Default::default()
        };
        assert_eq!(reg.resolve_process_chain(&on, &["ocr"]).len(), 1);
        assert_eq!(reg.resolve_process_chain(&off, &["ocr"]).len(), 0);
    }

    #[test]
    fn delegate_cycle_detected() {
        let mut reg = Registry::new();
        reg.register_parse(
            Arc::new(MockParse {
                id: "a",
                formats: vec!["x"],
                available: true,
            }),
            Some("b"),
        )
        .unwrap();
        let err = reg.register_parse(
            Arc::new(MockParse {
                id: "b",
                formats: vec!["x"],
                available: true,
            }),
            Some("a"),
        );
        assert!(err.is_err());
        // 自环同样拒绝
        let err2 = reg.register_parse(
            Arc::new(MockParse {
                id: "c",
                formats: vec!["x"],
                available: true,
            }),
            Some("c"),
        );
        assert!(err2.is_err());
    }
}
