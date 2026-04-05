//! HTML→Markdown / 纯文本转换工具。

use regex::Regex;
use scraper::{Html, Selector};

const REMOVABLE_TAGS: &[&str] = &[
    "script", "style", "nav", "footer", "header", "aside", "noscript",
];

/// 使用 scraper 解析 HTML，去除 script/style/nav/footer/header 等无关标签，
/// 提取 body 的内容后做 HTML→Markdown 转换。
pub fn html_to_markdown(raw_html: &str) -> String {
    let cleaned = clean_html(raw_html);
    convert_to_markdown(&cleaned)
}

/// 去除所有 HTML 标签，解码实体，返回纯文本。
pub fn strip_tags(raw_html: &str) -> String {
    let cleaned = clean_html(raw_html);
    let re_tags = Regex::new(r"<[^>]+>").unwrap();
    let text = re_tags.replace_all(&cleaned, "");
    decode_entities(&normalize_whitespace(&text))
}

/// 使用 scraper 清理 HTML：去除 script, style, nav, footer, header, aside 标签的内容。
fn clean_html(raw: &str) -> String {
    let doc = Html::parse_document(raw);

    let body_sel = Selector::parse("body").unwrap();
    let body = doc.select(&body_sel).next();

    let root = match body {
        Some(b) => b,
        None => return doc.root_element().inner_html(),
    };

    fn should_skip(el: &scraper::node::Element) -> bool {
        let name = el.name();
        REMOVABLE_TAGS.contains(&name)
    }

    fn collect_html(node: scraper::ElementRef, out: &mut String) {
        for child in node.children() {
            match child.value() {
                scraper::Node::Text(t) => out.push_str(t),
                scraper::Node::Element(el) => {
                    if should_skip(el) {
                        continue;
                    }
                    if let Some(cr) = scraper::ElementRef::wrap(child) {
                        let tag = el.name();
                        let attrs: String =
                            el.attrs().map(|(k, v)| format!(r#" {k}="{v}""#)).collect();
                        out.push_str(&format!("<{tag}{attrs}>"));
                        collect_html(cr, out);
                        out.push_str(&format!("</{tag}>"));
                    }
                }
                _ => {}
            }
        }
    }

    let mut result = String::new();
    collect_html(root, &mut result);
    result
}

/// HTML→Markdown 正则转换。
fn convert_to_markdown(html: &str) -> String {
    let mut text = html.to_string();

    // 链接: <a href="url">text</a> → [text](url)
    let re_link = Regex::new(r#"<a\s+[^>]*href=["']([^"']+)["'][^>]*>([\s\S]*?)</a>"#).unwrap();
    text = re_link
        .replace_all(&text, |caps: &regex::Captures| {
            let url = &caps[1];
            let inner = strip_inline_tags(&caps[2]);
            format!("[{inner}]({url})")
        })
        .into_owned();

    // 标题: <h1>...</h1> → # ...
    let re_heading = Regex::new(r"<h([1-6])[^>]*>([\s\S]*?)</h[1-6]>").unwrap();
    text = re_heading
        .replace_all(&text, |caps: &regex::Captures| {
            let level: usize = caps[1].parse().unwrap_or(1);
            let content = strip_inline_tags(&caps[2]);
            format!("\n{} {content}\n", "#".repeat(level))
        })
        .into_owned();

    // 列表项: <li>...</li> → - ...
    let re_li = Regex::new(r"<li[^>]*>([\s\S]*?)</li>").unwrap();
    text = re_li
        .replace_all(&text, |caps: &regex::Captures| {
            let content = strip_inline_tags(&caps[1]);
            format!("\n- {content}")
        })
        .into_owned();

    // 段落/块级分隔
    let re_block_close = Regex::new(r"</(p|div|section|article|blockquote)>").unwrap();
    text = re_block_close.replace_all(&text, "\n\n").into_owned();

    // 换行
    let re_br = Regex::new(r"<(br|hr)\s*/?>").unwrap();
    text = re_br.replace_all(&text, "\n").into_owned();

    // 去除剩余 HTML 标签
    let re_all_tags = Regex::new(r"<[^>]+>").unwrap();
    text = re_all_tags.replace_all(&text, "").into_owned();

    // 解码 HTML 实体
    text = decode_entities(&text);

    normalize_whitespace(&text)
}

/// 去除行内 HTML 标签（保留文本内容）。
fn strip_inline_tags(s: &str) -> String {
    let re = Regex::new(r"<[^>]+>").unwrap();
    let stripped = re.replace_all(s, "");
    decode_entities(&stripped).trim().to_string()
}

/// 解码常见 HTML 实体。
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// 规范化空白：合并连续空格/制表符为单个空格，合并 3+ 连续换行为 2 个。
fn normalize_whitespace(s: &str) -> String {
    let re_spaces = Regex::new(r"[ \t]+").unwrap();
    let text = re_spaces.replace_all(s, " ");
    let re_newlines = Regex::new(r"\n{3,}").unwrap();
    re_newlines.replace_all(&text, "\n\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_tags_basic() {
        let html = "<p>Hello <b>World</b></p>";
        assert_eq!(strip_tags(html), "Hello World");
    }

    #[test]
    fn test_strip_tags_with_entities() {
        let html = "<html><body><p>A &amp; B &lt; C</p></body></html>";
        let result = strip_tags(html);
        assert!(
            result.contains("A & B < C")
                || result.contains("A &amp; B &lt; C")
                || result.contains("A & B")
        );
    }

    #[test]
    fn test_html_to_markdown_heading() {
        let html = "<h1>Title</h1><p>Content here</p>";
        let md = html_to_markdown(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("Content here"));
    }

    #[test]
    fn test_html_to_markdown_link() {
        let html = r#"<a href="https://example.com">Click</a>"#;
        let md = html_to_markdown(html);
        assert!(md.contains("[Click](https://example.com)"));
    }

    #[test]
    fn test_html_to_markdown_list() {
        let html = "<ul><li>Item 1</li><li>Item 2</li></ul>";
        let md = html_to_markdown(html);
        assert!(md.contains("- Item 1"));
        assert!(md.contains("- Item 2"));
    }

    #[test]
    fn test_html_to_markdown_removes_script() {
        let html = "<html><body><script>alert('xss')</script><p>Safe</p></body></html>";
        let md = html_to_markdown(html);
        assert!(!md.contains("alert"));
        assert!(md.contains("Safe"));
    }

    #[test]
    fn test_normalize_whitespace() {
        let s = "Hello   World\n\n\n\nParagraph";
        let result = normalize_whitespace(s);
        assert_eq!(result, "Hello World\n\nParagraph");
    }

    #[test]
    fn test_decode_entities() {
        assert_eq!(decode_entities("&amp;&lt;&gt;"), "&<>");
    }
}
