use std::collections::HashSet;

use ego_tree::NodeId;
use once_cell::sync::Lazy;
use scraper::{ElementRef, Node, Selector};
use url::Url;

use crate::noise;
use crate::types::{CodeBlock, Image, Link};

const MAX_DOM_DEPTH: usize = 200;

static A_HREF: Lazy<Selector> = Lazy::new(|| Selector::parse("a[href]").unwrap());
static IMG: Lazy<Selector> = Lazy::new(|| Selector::parse("img").unwrap());
static PRE_CODE: Lazy<Selector> = Lazy::new(|| Selector::parse("pre code").unwrap());

const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

#[derive(Default)]
pub struct ConvertedAssets {
    pub links: Vec<Link>,
    pub images: Vec<Image>,
    pub code_blocks: Vec<CodeBlock>,
}

pub fn resolve_url(href: &str, base_url: Option<&Url>) -> String {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match base_url {
        Some(base) => base
            .join(trimmed)
            .map(String::from)
            .unwrap_or_else(|_| trimmed.to_string()),
        None => trimmed.to_string(),
    }
}

pub fn convert(
    element: ElementRef<'_>,
    base_url: Option<&Url>,
    exclude: &HashSet<NodeId>,
) -> (String, String, ConvertedAssets) {
    let mut html = String::new();
    write_element(element, base_url, exclude, &mut html, 0);

    let markdown = htmd::convert(&html).unwrap_or_default().trim().to_string();
    let plain_text = collapse_whitespace(&strip_markdown(&markdown));
    let assets = collect_assets(element, base_url, exclude);

    (markdown, plain_text, assets)
}

fn keep(node: ego_tree::NodeRef<'_, Node>, exclude: &HashSet<NodeId>) -> bool {
    if exclude.contains(&node.id()) {
        return false;
    }
    match ElementRef::wrap(node) {
        Some(element) => !noise::is_noise(element),
        None => true,
    }
}

fn write_element(
    element: ElementRef<'_>,
    base_url: Option<&Url>,
    exclude: &HashSet<NodeId>,
    out: &mut String,
    depth: usize,
) {
    if depth > MAX_DOM_DEPTH {
        return;
    }

    let name = element.value().name();
    out.push('<');
    out.push_str(name);
    for (key, value) in element.value().attrs() {
        let resolved = match key {
            "href" | "src" => resolve_url(value, base_url),
            _ => value.to_string(),
        };
        out.push(' ');
        out.push_str(key);
        out.push_str("=\"");
        out.push_str(&escape_attr(&resolved));
        out.push('"');
    }
    out.push('>');

    if VOID_TAGS.contains(&name) {
        return;
    }

    for child in element.children() {
        if !keep(child, exclude) {
            continue;
        }
        match child.value() {
            Node::Text(text) => out.push_str(&escape_text(text)),
            Node::Element(_) => {
                if let Some(child_element) = ElementRef::wrap(child) {
                    write_element(child_element, base_url, exclude, out, depth + 1);
                }
            }
            _ => {}
        }
    }

    out.push_str("</");
    out.push_str(name);
    out.push('>');
}

fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(value: &str) -> String {
    escape_text(value).replace('"', "&quot;")
}

fn collect_assets(
    element: ElementRef<'_>,
    base_url: Option<&Url>,
    exclude: &HashSet<NodeId>,
) -> ConvertedAssets {
    let mut assets = ConvertedAssets::default();
    let visible = |node: ElementRef<'_>| {
        !exclude.contains(&node.id()) && !noise::is_noise(node) && !noise::is_noise_descendant(node)
    };

    for anchor in element.select(&A_HREF) {
        if !visible(anchor) {
            continue;
        }
        let text = collapse_whitespace(&anchor.text().collect::<String>());
        let href = resolve_url(anchor.value().attr("href").unwrap_or_default(), base_url);
        if !text.is_empty() && !href.is_empty() {
            assets.links.push(Link { text, href });
        }
    }

    for image in element.select(&IMG) {
        if !visible(image) {
            continue;
        }
        let src = image
            .value()
            .attr("src")
            .or_else(|| image.value().attr("data-src"))
            .unwrap_or_default();
        let src = resolve_url(src, base_url);
        if !src.is_empty() {
            assets.images.push(Image {
                alt: image.value().attr("alt").unwrap_or_default().to_string(),
                src,
            });
        }
    }

    for code in element.select(&PRE_CODE) {
        if !visible(code) {
            continue;
        }
        let body = code.text().collect::<String>();
        if body.trim().is_empty() {
            continue;
        }
        assets.code_blocks.push(CodeBlock {
            language: code.value().attr("class").and_then(language_from_class),
            code: body,
        });
    }

    assets
}

fn language_from_class(class: &str) -> Option<String> {
    class.split_whitespace().find_map(|token| {
        token
            .strip_prefix("language-")
            .or_else(|| token.strip_prefix("lang-"))
            .filter(|name| !name.is_empty())
            .map(str::to_lowercase)
    })
}

fn strip_markdown(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut chars = markdown.chars().peekable();
    while let Some(letter) = chars.next() {
        match letter {
            '[' => {}
            ']' => {
                if chars.peek() == Some(&'(') {
                    for skipped in chars.by_ref() {
                        if skipped == ')' {
                            break;
                        }
                    }
                }
            }
            '#' | '*' | '_' | '`' | '>' => {}
            '!' if chars.peek() == Some(&'[') => {}
            other => out.push(other),
        }
    }
    out
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blanks += 1;
            if blanks <= 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blanks = 0;
            out.push_str(&trimmed.split_whitespace().collect::<Vec<_>>().join(" "));
            out.push('\n');
        }
    }
    out.trim().to_string()
}
