//! HTML to Markdown for agents.
//!
//! Canvas stores pages and announcements as HTML. Agents read Markdown far
//! more cheaply, so bodies are converted with `htmd`. Embedded players
//! (`<iframe>`, notably Panopto ReCap) would otherwise vanish, so each is
//! first rewritten into a plain link that keeps its `src`.

use std::sync::LazyLock;

use regex::Regex;

/// Matches an `<iframe ...>` opening tag (and an immediately following
/// close tag) and captures its `src`, quoted or not. `data-src` does not
/// count. The `regex` crate is linear-time, so hostile HTML cannot make it
/// backtrack.
static IFRAME: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)<iframe\b[^>]*?[\s"']src\s*=\s*(?:"([^"]+)"|'([^']+)'|([^\s>"']+))[^>]*>(?:\s*</iframe>)?"#,
    )
    .ok()
});

/// Converts Canvas HTML to Markdown, keeping embedded players as links.
///
/// # Example
/// ```
/// let md = canvas_api::html_to_markdown(
///     r#"<h2>Week 1</h2><iframe src="https://example.hosted.panopto.com/Panopto/Pages/Embed.aspx?id=abc"></iframe>"#,
/// );
/// assert!(md.contains("## Week 1"));
/// assert!(md.contains("Embed.aspx?id=abc"));
/// ```
pub fn html_to_markdown(html: &str) -> String {
    let html = match IFRAME.as_ref() {
        Some(re) => re
            .replace_all(html, |c: &regex::Captures<'_>| {
                let src = c
                    .get(1)
                    .or_else(|| c.get(2))
                    .or_else(|| c.get(3))
                    .map_or("", |m| m.as_str());
                format!(r#"<p><a href="{src}">Embedded media: {src}</a></p>"#)
            })
            .into_owned(),
        None => html.to_owned(),
    };
    htmd::convert(&html)
        .unwrap_or_else(|_| html.clone())
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_headings_lists_and_links() {
        let md = html_to_markdown(
            r#"<h1>Intro</h1><ul><li>One</li><li><a href="https://x/y">Two</a></li></ul>"#,
        );
        assert!(md.contains("# Intro"));
        assert!(md.contains("One"));
        assert!(md.contains("[Two](https://x/y)"));
    }

    #[test]
    fn unclosed_iframe_does_not_swallow_later_content() {
        let md = html_to_markdown(
            r#"<iframe src="https://p/1"><p>Keep me</p><iframe src="https://p/2"></iframe>"#,
        );
        assert!(md.contains("Keep me"), "{md}");
        assert!(md.contains("https://p/1") && md.contains("https://p/2"));
    }

    #[test]
    fn unquoted_src_and_data_src_first() {
        let md =
            html_to_markdown(r#"<iframe data-src="https://lazy" src=https://real/x></iframe>"#);
        assert!(md.contains("https://real/x"), "{md}");
    }

    #[test]
    fn rewrites_iframes_with_either_quote_style() {
        let md = html_to_markdown("<iframe width='1' src='https://p/e?id=1'></iframe>");
        assert!(md.contains("https://p/e?id=1"));
    }
}
