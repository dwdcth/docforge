// 吸收自 firecrawl/anydoc 0.2.5（MIT，`src/shared/math/`），见 THIRD_PARTY_NOTICES.md。
//! 数学公式转 LaTeX：OMML（DOCX/PPTX）与 Presentation MathML（HTML/EPUB/ODF）。
//!
//! 零第三方依赖，可直接在所有 feature 组合下工作。

pub mod mathml;
pub mod omml;
mod tex;
pub mod xml;

pub use mathml::{mathml_is_display, mathml_to_tex};
pub use omml::{omath_para_to_tex, omath_to_tex};

/// 解析 `<m:oMath>` 或 `<m:oMathPara>` XML 字符串为 LaTeX（无定界符）。
pub fn omath_xml_to_tex(src: &str) -> String {
    let root = xml::parse_xml(src);
    let Some(first) = root.child_elems().next() else {
        return String::new();
    };
    if first.local == "oMathPara" {
        omath_para_to_tex(first).join(" \\\\ ")
    } else {
        omath_to_tex(first)
    }
}

/// 解析 `<math>` XML 字符串为 `(latex, is_display)`。
pub fn mathml_xml_to_tex(src: &str) -> (String, bool) {
    let root = xml::parse_xml(src);
    let Some(first) = root.child_elems().next() else {
        return (String::new(), false);
    };
    (mathml_to_tex(first), mathml_is_display(first))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn omml(body: &str) -> String {
        let src = format!(
            r#"<m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">{body}</m:oMath>"#
        );
        omath_xml_to_tex(&src)
    }

    fn mathml(body: &str) -> String {
        let src = format!(r#"<math xmlns="http://www.w3.org/1998/Math/MathML">{body}</math>"#);
        mathml_xml_to_tex(&src).0
    }

    #[test]
    fn omml_structures_map_to_latex_commands() {
        let tex = omml(concat!(
            "<m:f><m:num><m:r><m:t>a</m:t></m:r></m:num><m:den><m:r><m:t>b</m:t></m:r></m:den></m:f>",
            "<m:sSup><m:e><m:r><m:t>x</m:t></m:r></m:e><m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSup>",
            "<m:rad><m:radPr><m:degHide m:val=\"1\"/></m:radPr><m:deg/><m:e><m:r><m:t>y</m:t></m:r></m:e></m:rad>",
            "<m:nary><m:naryPr><m:chr m:val=\"∑\"/><m:limLoc m:val=\"undOvr\"/></m:naryPr>",
            "<m:sub><m:r><m:t>i=1</m:t></m:r></m:sub><m:sup><m:r><m:t>n</m:t></m:r></m:sup>",
            "<m:e><m:r><m:t>i</m:t></m:r></m:e></m:nary>",
            "<m:d><m:dPr><m:begChr m:val=\"⟨\"/><m:endChr m:val=\"⟩\"/></m:dPr><m:e><m:r><m:t>α</m:t></m:r></m:e></m:d>",
            "<m:func><m:fName><m:r><m:rPr><m:sty m:val=\"p\"/></m:rPr><m:t>sin</m:t></m:r></m:fName>",
            "<m:e><m:r><m:t>θ</m:t></m:r></m:e></m:func>",
        ));
        assert_eq!(
            tex,
            "\\frac{a}{b}x^{2}\\sqrt{y}\\sum_{i=1}^{n}{i}\\left\\langle\\alpha\\right\\rangle\\sin{\\theta}"
        );
    }

    #[test]
    fn omml_normal_text_and_styled_alphanumerics() {
        let tex = omml(concat!(
            "<m:r><m:rPr><m:nor/></m:rPr><m:t>for all </m:t></m:r>",
            "<m:r><m:t>𝐱 ∈ ℝ</m:t></m:r>",
        ));
        assert_eq!(tex, "\\text{for all }\\mathbf{x} \\in \\mathbb{R}");
    }

    #[test]
    fn omml_fragment_without_xmlns_defaults_m_prefix() {
        let tex = omath_xml_to_tex(
            "<m:oMath><m:f><m:num><m:r><m:t>1</m:t></m:r></m:num><m:den><m:r><m:t>2</m:t></m:r></m:den></m:f></m:oMath>",
        );
        assert_eq!(tex, "\\frac{1}{2}");
    }

    #[test]
    fn omml_rtf_shaped_tree_reads_values_from_text() {
        let tex = omml(concat!(
            "<m:f><m:fPr><m:type>lin</m:type></m:fPr>",
            "<m:num><m:r>a</m:r></m:num><m:den><m:r><m:sty>0</m:sty>b</m:r></m:den></m:f>",
        ));
        assert_eq!(tex, "{a}/{\\mathrm{b}}");
    }

    #[test]
    fn mathml_prefers_a_tex_annotation_and_drops_the_rest() {
        let tex = mathml(concat!(
            "<semantics><mrow><mi>x</mi><mo>=</mo><mn>1</mn></mrow>",
            "<annotation encoding=\"application/x-tex\">x = 1</annotation>",
            "<annotation encoding=\"StarMath 5.0\">x = 1</annotation></semantics>",
        ));
        assert_eq!(tex, "x = 1");
    }

    #[test]
    fn mathml_presentation_tree_converts() {
        let tex = mathml(concat!(
            "<mrow><mi>f</mi><mo>⁡</mo><mfenced><mi>x</mi></mfenced><mo>=</mo>",
            "<munderover><mo>∑</mo><mrow><mi>k</mi><mo>=</mo><mn>0</mn></mrow><mi>∞</mi></munderover>",
            "<mfrac><msup><mi>x</mi><mi>k</mi></msup><mrow><mi>k</mi><mo>!</mo></mrow></mfrac>",
            "<mo>+</mo><msqrt><mi>π</mi></msqrt>",
            "<mover><mi>v</mi><mo>→</mo></mover><mi>sin</mi><mtext>for</mtext></mrow>",
        ));
        assert_eq!(
            tex,
            "f\\left(x\\right)=\\sum_{k=0}^{\\infty}\\frac{x^{k}}{k!}+\\sqrt{\\pi}\\vec{v}\\sin\\text{for}"
        );
    }

    #[test]
    fn mathml_display_attribute_selects_block_layout() {
        let (tex, disp) = mathml_xml_to_tex(
            r#"<math xmlns="http://www.w3.org/1998/Math/MathML" display="block"><mi>x</mi></math>"#,
        );
        assert_eq!(tex, "x");
        assert!(disp);
    }
}
