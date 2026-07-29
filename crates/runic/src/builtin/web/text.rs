/// Drop `<tag …>…</tag>` sections (case-insensitive) wholesale — used to strip
/// `<script>`/`<style>` whose contents are not prose.
fn remove_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let (mut rest, mut lrest) = (html, lower.as_str());
    loop {
        let Some(start) = lrest.find(&open) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        match lrest[start..].find(&close) {
            Some(rel) => {
                let end = start + rel + close.len();
                rest = &rest[end..];
                lrest = &lrest[end..];
            }
            None => break, // unterminated → drop the rest
        }
    }
    out
}

/// Decode HTML entities (`&amp;`, `&#39;`, `&#x41;`, …). Pure; exposed for
/// fuzzing — must never panic on arbitrary input.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // Find the entity terminator, but only accept short entities (≤12
        // bytes). `find` returns a char-safe byte index — slicing `tail[..12]`
        // directly panics when byte 12 lands inside a multi-byte char.
        let Some(semi) = tail.find(';').filter(|&p| p <= 12) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| {
                    n.strip_prefix(['x', 'X'])
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .or_else(|| n.parse::<u32>().ok())
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapse trailing per-line whitespace and runs of >1 blank line.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            blanks += 1;
            if blanks <= 1 {
                out.push('\n');
            }
        } else {
            blanks = 0;
            out.push_str(t.trim_start());
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// A small, dependency-free HTML→text pass: strip script/style, turn
/// block-level tags into line breaks, drop the rest of the markup, decode
/// entities. Good enough for a model to read; not a full renderer. Pure;
/// exposed for fuzzing — must never panic on arbitrary input.
pub fn html_to_text(html: &str) -> String {
    let cleaned = remove_blocks(&remove_blocks(html, "script"), "style");
    let mut out = String::with_capacity(cleaned.len());
    let mut tag = String::new();
    let mut in_tag = false;
    for c in cleaned.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let name = tag
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(
                    name.as_str(),
                    "p" | "br"
                        | "div"
                        | "li"
                        | "tr"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "ul"
                        | "ol"
                        | "table"
                        | "blockquote"
                        | "pre"
                        | "hr"
                ) {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    collapse_ws(&decode_entities(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_markup_and_decodes() {
        let html = "<html><head><style>.x{color:red}</style></head>\
            <body><h1>Title</h1><script>alert(1)</script>\
            <p>Hello &amp; welcome to <b>runic</b>.</p>\
            <ul><li>one</li><li>two</li></ul></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome to runic."));
        assert!(text.contains("one"));
        assert!(text.contains("two"));
        // script/style contents gone
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    fn entity_decoding_handles_numeric() {
        assert_eq!(decode_entities("a&#65;b"), "aAb");
        assert_eq!(decode_entities("&#x41;"), "A");
        assert_eq!(
            decode_entities("plain &unknown; text"),
            "plain &unknown; text"
        );
    }

    #[test]
    fn entity_decoding_is_char_boundary_safe() {
        // Regression: byte 12 after the `&` lands inside the multi-byte 'à'.
        // The old `tail[..12]` slice panicked here.
        assert_eq!(
            decode_entities("&#039;aide à la décision"),
            "'aide à la décision"
        );
        // A bare `&` followed by multibyte text and no nearby ';' must not panic.
        assert_eq!(
            decode_entities("R&D coûte à la société"),
            "R&D coûte à la société"
        );
    }
}
