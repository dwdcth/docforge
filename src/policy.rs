// 吸收自 docparser-core 0.2.1（MIT OR Apache-2.0，crates.io 存档），见 THIRD_PARTY_NOTICES.md。
// docforge 改动：仅 crate 名引用替换。
//! 后端选择策略（与 Python `policy.py` 对齐）。

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// 只用自带后端，不依赖外部进程（默认，保证独立性）
    #[default]
    Minimal,
    /// 允许外部后端（如 LibreOffice）作为回退
    Compatibility,
    /// 追求效果，启用全部可选后端
    Agent,
}

#[derive(Debug, Clone, Default)]
pub struct BackendPolicy {
    pub profile: Profile,
    pub preferred: Option<String>,
    pub chain: Vec<String>,
    pub allow_fallback: bool,
    pub timeout_secs: f64,
    pub enable_ocr: bool,
    pub enable_layout: bool,
    pub ocr_dpi: u32,
    pub ocr_drop_score: f32,
    pub max_pages: usize,
}

impl BackendPolicy {
    /// 计算实际尝试顺序：显式 chain > preferred 置顶 > 默认链。
    pub fn ordered_chain(&self, default_chain: &[String]) -> Vec<String> {
        let mut base: Vec<String> = if !self.chain.is_empty() {
            self.chain.clone()
        } else {
            let mut d = default_chain.to_vec();
            if let Some(pref) = &self.preferred {
                if let Some(pos) = d.iter().position(|x| x == pref) {
                    d.remove(pos);
                    d.insert(0, pref.clone());
                } else {
                    d.insert(0, pref.clone());
                }
            }
            d
        };
        if !self.allow_fallback && base.len() > 1 {
            base.truncate(1);
        }
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn preferred_promotes() {
        let p = BackendPolicy {
            preferred: Some("b".into()),
            allow_fallback: true,
            ..Default::default()
        };
        assert_eq!(p.ordered_chain(&s(&["a", "b", "c"])), s(&["b", "a", "c"]));
    }

    #[test]
    fn explicit_chain_wins() {
        let p = BackendPolicy {
            chain: s(&["c", "a"]),
            allow_fallback: true,
            ..Default::default()
        };
        assert_eq!(p.ordered_chain(&s(&["a", "b", "c"])), s(&["c", "a"]));
    }

    #[test]
    fn no_fallback_truncates() {
        let p = BackendPolicy {
            allow_fallback: false,
            ..Default::default()
        };
        assert_eq!(p.ordered_chain(&s(&["a", "b"])), s(&["a"]));
    }
}
