//! 轻量 XML 片段树（零第三方依赖），专供 OMML / MathML 公式转换使用。
//!
//! 同时提供 `quick-xml` 流式捕获入口（在 docx/pptx/odt 流式扫描遇到 `<m:oMath>` /
//! `<math:math>` 时直接构造子树，免二次解析），并统一施加 [`crate::limits::MAX_XML_DEPTH`]
//! 与 [`crate::limits::MAX_XML_NODES`] 安全上限。

use crate::limits::{MAX_XML_DEPTH, MAX_XML_NODES};

pub mod ns {
    pub const M: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";
    pub const M_STRICT: &str = "http://purl.oclc.org/ooxml/officeDocument/math";
    pub const MATHML: &str = "http://www.w3.org/1998/Math/MathML";
}

#[derive(Debug, Clone)]
pub enum Node {
    Elem(Element),
    Text(String),
}

#[derive(Debug, Clone, Default)]
pub struct Element {
    pub ns: Option<String>,
    pub local: String,
    pub attrs: Vec<(Option<String>, String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn child_elems(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Elem(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    pub fn find(&self, namespace: &str, local: &str) -> Option<&Element> {
        self.child_elems()
            .find(|e| e.local == local && e.ns.as_deref() == Some(namespace))
    }

    pub fn find_all<'a>(
        &'a self,
        namespace: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a Element> {
        self.child_elems()
            .filter(move |e| e.local == local && e.ns.as_deref() == Some(namespace))
    }

    pub fn attr(&self, namespace: &str, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(a_ns, a_loc, _)| {
                a_loc == local && (a_ns.as_deref() == Some(namespace) || a_ns.is_none())
            })
            .map(|(_, _, v)| v.as_str())
    }

    pub fn attr_any(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(_, a_loc, _)| a_loc == local)
            .map(|(_, _, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        collect_text(self, &mut out);
        out
    }
}

fn collect_text(e: &Element, out: &mut String) {
    for n in &e.children {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Elem(c) => collect_text(c, out),
        }
    }
}

fn normalize_ns(uri: &str) -> String {
    if uri == ns::M_STRICT {
        ns::M.to_string()
    } else {
        uri.to_string()
    }
}

fn split_qname(qname: &str) -> (Option<&str>, &str) {
    match qname.split_once(':') {
        Some((p, l)) => (Some(p), l),
        None => (None, qname),
    }
}

/// 当从外层文档截取 `<m:oMath>` 片段时，外层根元素的 `xmlns:m` 可能不在片段内；
/// 对已知公式前缀做规范默认映射，保证片段独立解析也与整文档解析一致。
fn resolve_prefix(scopes: &[Vec<(Option<String>, String)>], prefix: Option<&str>) -> Option<String> {
    for scope in scopes.iter().rev() {
        for (p, uri) in scope.iter().rev() {
            if p.as_deref() == prefix {
                return Some(uri.clone());
            }
        }
    }
    match prefix {
        Some("m") => Some(ns::M.to_string()),
        Some("math") | Some("mml") => Some(ns::MATHML.to_string()),
        _ => None,
    }
}

fn decode_xml_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(rel) = s[i..].find(';') {
                if rel <= 16 {
                    let body = &s[i + 1..i + rel];
                    let decoded = match body {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        "nbsp" => Some('\u{a0}'),
                        _ => body.strip_prefix('#').and_then(|rest| {
                            let code = match rest.strip_prefix(['x', 'X']) {
                                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                                None => rest.parse::<u32>().ok()?,
                            };
                            char::from_u32(code)
                        }),
                    };
                    if let Some(ch) = decoded {
                        out.push(ch);
                        i += rel + 1;
                        continue;
                    }
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 解析单个 XML 片段为合成根 `Element`（子节点即片段顶层元素）。
/// 零第三方依赖，用于 HTML/EPUB 的 `<math>` 片段及单测；触及深度/节点上限安全中止。
pub fn parse_xml(xml: &str) -> Element {
    let mut root = Element::default();
    let mut stack: Vec<Element> = Vec::new();
    let mut scopes: Vec<Vec<(Option<String>, String)>> = Vec::new();
    let mut nodes = 0usize;

    let bytes = xml.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && nodes < MAX_XML_NODES {
        if bytes[i] != b'<' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            let text = decode_xml_entities(&xml[start..i]);
            if !text.is_empty() {
                nodes += 1;
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node::Text(text));
                } else {
                    root.children.push(Node::Text(text));
                }
            }
            continue;
        }
        // 注释
        if xml[i..].starts_with("<!--") {
            i = match xml[i + 4..].find("-->") {
                Some(rel) => i + 4 + rel + 3,
                None => bytes.len(),
            };
            continue;
        }
        // CDATA
        if xml[i..].starts_with("<![CDATA[") {
            let end = xml[i + 9..].find("]]>").map(|r| i + 9 + r);
            let body = match end {
                Some(e) => {
                    let s = &xml[i + 9..e];
                    i = e + 3;
                    s
                }
                None => {
                    let s = &xml[i + 9..];
                    i = bytes.len();
                    s
                }
            };
            if !body.is_empty() {
                nodes += 1;
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node::Text(body.to_string()));
                }
            }
            continue;
        }
        // 处理指令 / DOCTYPE
        if xml[i..].starts_with("<?") {
            i = match xml[i + 2..].find("?>") {
                Some(rel) => i + 2 + rel + 2,
                None => bytes.len(),
            };
            continue;
        }
        if xml[i..].starts_with("<!") {
            i = match xml[i + 2..].find('>') {
                Some(rel) => i + 2 + rel + 1,
                None => bytes.len(),
            };
            continue;
        }

        // 查找引号感知的标签结束 `>`
        let Some(tag_end_rel) = find_tag_end(&xml[i + 1..]) else {
            break;
        };
        let raw = xml[i + 1..i + 1 + tag_end_rel].trim();
        i += tag_end_rel + 2;

        if let Some(close_name) = raw.strip_prefix('/') {
            let (_, close_local) = split_qname(close_name.trim());
            if let Some(pos) = stack.iter().rposition(|e| e.local == close_local) {
                while stack.len() > pos {
                    let done = stack.pop().unwrap();
                    scopes.pop();
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(Node::Elem(done));
                    } else {
                        root.children.push(Node::Elem(done));
                    }
                }
            }
            continue;
        }

        let self_closing = raw.ends_with('/');
        let body = if self_closing {
            raw[..raw.len() - 1].trim_end()
        } else {
            raw
        };
        if body.is_empty() {
            continue;
        }

        let (qname, attr_str) = match body.find(|c: char| c.is_whitespace()) {
            Some(p) => (&body[..p], &body[p..]),
            None => (body, ""),
        };
        let raw_attrs = parse_attrs(attr_str);
        let mut scope = Vec::new();
        for (k, v) in &raw_attrs {
            if k == "xmlns" {
                scope.push((None, normalize_ns(v)));
            } else if let Some(prefix) = k.strip_prefix("xmlns:") {
                scope.push((Some(prefix.to_string()), normalize_ns(v)));
            }
        }
        scopes.push(scope);

        let (elem_prefix, elem_local) = split_qname(qname);
        let elem_ns = resolve_prefix(&scopes, elem_prefix);
        let mut attrs = Vec::new();
        for (k, v) in raw_attrs {
            if k == "xmlns" || k.starts_with("xmlns:") {
                continue;
            }
            let (ap, al) = split_qname(&k);
            let ans = ap.and_then(|p| resolve_prefix(&scopes, Some(p)));
            attrs.push((ans, al.to_string(), v));
        }

        let elem = Element {
            ns: elem_ns,
            local: elem_local.to_string(),
            attrs,
            children: Vec::new(),
        };
        nodes += 1;
        if self_closing || stack.len() >= MAX_XML_DEPTH {
            scopes.pop();
            if let Some(parent) = stack.last_mut() {
                parent.children.push(Node::Elem(elem));
            } else {
                root.children.push(Node::Elem(elem));
            }
        } else {
            stack.push(elem);
        }
    }

    while let Some(done) = stack.pop() {
        scopes.pop();
        if let Some(parent) = stack.last_mut() {
            parent.children.push(Node::Elem(done));
        } else {
            root.children.push(Node::Elem(done));
        }
    }
    root
}

fn find_tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (idx, &b) in s.as_bytes().iter().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(idx),
                _ => {}
            },
        }
    }
    None
}

fn parse_attrs(mut s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    loop {
        s = s.trim_start();
        if s.is_empty() {
            break;
        }
        let name_end = s
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(s.len());
        let name = &s[..name_end];
        if name.is_empty() {
            break;
        }
        s = s[name_end..].trim_start();
        if let Some(after_eq) = s.strip_prefix('=') {
            s = after_eq.trim_start();
            let (val, rest) = if let Some(v) = s.strip_prefix('"') {
                match v.find('"') {
                    Some(end) => (&v[..end], &v[end + 1..]),
                    None => (v, ""),
                }
            } else if let Some(v) = s.strip_prefix('\'') {
                match v.find('\'') {
                    Some(end) => (&v[..end], &v[end + 1..]),
                    None => (v, ""),
                }
            } else {
                let end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
                (&s[..end], &s[end..])
            };
            out.push((name.to_string(), decode_xml_entities(val)));
            s = rest;
        } else {
            out.push((name.to_string(), String::new()));
        }
    }
    out
}

/// 从活跃的 `quick_xml::Reader` 流式捕获当前起始标签对应的完整子树为 `Element`
/// （调用方刚消费 `Event::Start(start)`，返回时 reader 停在对应闭合 `Event::End` 之后）。
#[cfg(any(
    feature = "docx",
    feature = "pptx",
    feature = "odt",
    feature = "ods",
    feature = "odp",
    feature = "epub",
    feature = "fb2"
))]
pub(crate) fn capture_from_quick_xml<R: std::io::BufRead>(
    reader: &mut quick_xml::Reader<R>,
    start: &quick_xml::events::BytesStart<'_>,
) -> crate::error::Result<Element> {
    use crate::error::DocError;
    use quick_xml::events::{BytesStart, Event};

    fn build_elem(
        e: &BytesStart<'_>,
        scopes: &mut Vec<Vec<(Option<String>, String)>>,
    ) -> Element {
        let mut scope = Vec::new();
        let mut raw_attrs = Vec::new();
        for a in e.attributes().flatten() {
            let key = a.key.as_ref().to_string();
            let val = quick_xml::escape::unescape(&a.value)
                .map(|v| v.into_owned())
                .unwrap_or_else(|_| a.value.to_string());
            if key == "xmlns" {
                scope.push((None, normalize_ns(&val)));
            } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                scope.push((Some(prefix.to_string()), normalize_ns(&val)));
            } else {
                raw_attrs.push((key, val));
            }
        }
        scopes.push(scope);

        let qname = e.name().as_ref().to_string();
        let (ep, el) = split_qname(&qname);
        let elem_ns = resolve_prefix(scopes, ep);
        let mut attrs = Vec::with_capacity(raw_attrs.len());
        for (k, v) in raw_attrs {
            let (ap, al) = split_qname(&k);
            let ans = ap.and_then(|p| resolve_prefix(scopes, Some(p)));
            attrs.push((ans, al.to_string(), v));
        }
        Element {
            ns: elem_ns,
            local: el.to_string(),
            attrs,
            children: Vec::new(),
        }
    }

    let mut scopes: Vec<Vec<(Option<String>, String)>> = Vec::new();
    let root_elem = build_elem(start, &mut scopes);
    let mut stack: Vec<Element> = vec![root_elem];
    let mut buf = Vec::new();
    let mut nodes = 1usize;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if stack.len() >= MAX_XML_DEPTH || nodes >= MAX_XML_NODES {
                    return Err(DocError::resource_limit("公式 XML 嵌套或节点数超安全上限"));
                }
                nodes += 1;
                let elem = build_elem(e, &mut scopes);
                stack.push(elem);
            }
            Ok(Event::Empty(ref e)) => {
                if nodes >= MAX_XML_NODES {
                    return Err(DocError::resource_limit("公式 XML 节点数超安全上限"));
                }
                nodes += 1;
                let elem = build_elem(e, &mut scopes);
                scopes.pop();
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node::Elem(elem));
                }
            }
            Ok(Event::End(_)) => {
                let done = stack.pop().unwrap();
                scopes.pop();
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(Node::Elem(done));
                } else {
                    return Ok(done);
                }
            }
            Ok(Event::Text(ref e)) => {
                let raw = e.xml10_content();
                let t = quick_xml::escape::unescape(&raw)
                    .map(|v| v.into_owned())
                    .unwrap_or_else(|_| raw.into_owned());
                if !t.is_empty() {
                    if let Some(top) = stack.last_mut() {
                        top.children.push(Node::Text(t));
                    }
                }
            }
            Ok(Event::GeneralRef(ref e)) => {
                let piece = match e.resolve_char_ref() {
                    Ok(Some(ch)) => Some(ch.to_string()),
                    _ => {
                        let entity = format!("&{};", e.xml10_content());
                        quick_xml::escape::unescape(&entity)
                            .ok()
                            .map(|v| v.into_owned())
                    }
                };
                if let (Some(p), Some(top)) = (piece, stack.last_mut()) {
                    top.children.push(Node::Text(p));
                }
            }
            Ok(Event::CData(ref e)) => {
                if let Some(top) = stack.last_mut() {
                    top.children.push(Node::Text(e.as_ref().to_string()));
                }
            }
            Ok(Event::Eof) => {
                while stack.len() > 1 {
                    let done = stack.pop().unwrap();
                    scopes.pop();
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(Node::Elem(done));
                    }
                }
                return Ok(stack.pop().unwrap_or_default());
            }
            Err(e) => return Err(DocError::corrupt(format!("公式 XML 解析失败: {e}"))),
            _ => {}
        }
        buf.clear();
    }
}
