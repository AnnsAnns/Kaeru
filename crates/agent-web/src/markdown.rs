//! Markdown rendering for the web frontend (M2.5, ADR-026): the *only* place
//! that turns assistant Markdown into HTML. Safety is structural: we walk the
//! parser's events and emit an explicit allow-list, so raw HTML and dangerous
//! link schemes are never emitted. `agent-core` never sees HTML (C2).

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Render Markdown to a safe HTML fragment.
pub fn render(markdown: &str) -> String {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut emitter = Emitter::default();
    for event in Parser::new_ext(markdown, options) {
        emitter.event(event);
    }
    emitter.finish()
}

#[derive(Default)]
struct Emitter {
    out: String,
    /// One entry per open `<a>`: did we actually emit the tag (safe scheme)?
    open_links: Vec<bool>,
    /// Inside a table head, cells become `<th>` instead of `<td>`.
    in_table_head: bool,
}

impl Emitter {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => escape_into(&mut self.out, &text),
            Event::Code(code) => {
                self.out.push_str("<code>");
                escape_into(&mut self.out, &code);
                self.out.push_str("</code>");
            }
            Event::SoftBreak => self.out.push('\n'),
            Event::HardBreak => self.out.push_str("<br>\n"),
            Event::Rule => self.out.push_str("<hr>\n"),
            Event::TaskListMarker(checked) => {
                if checked {
                    self.out
                        .push_str("<input type=\"checkbox\" disabled checked> ");
                } else {
                    self.out.push_str("<input type=\"checkbox\" disabled> ");
                }
            }
            // Raw HTML, math, footnotes and metadata are not part of the
            // allow-list: dropping them is what keeps the output inert.
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.out.push_str("<p>"),
            Tag::Heading { level, .. } => {
                self.out.push('<');
                self.out.push_str(heading_tag(level));
                self.out.push('>');
            }
            Tag::BlockQuote(_) => self.out.push_str("<blockquote>\n"),
            Tag::CodeBlock(kind) => {
                self.out.push_str("<pre><code");
                if let CodeBlockKind::Fenced(language) = kind {
                    // Keep the language as a class only when it is a plain
                    // identifier (never attribute-injectable).
                    let language = language.trim();
                    if !language.is_empty()
                        && language
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
                    {
                        self.out.push_str(" class=\"language-");
                        self.out.push_str(language);
                        self.out.push('"');
                    }
                }
                self.out.push('>');
            }
            Tag::HtmlBlock => {}
            Tag::List(start) => match start {
                Some(1) => self.out.push_str("<ol>"),
                Some(number) => {
                    self.out.push_str("<ol start=\"");
                    self.out.push_str(&number.to_string());
                    self.out.push_str("\">");
                }
                None => self.out.push_str("<ul>"),
            },
            Tag::Item => self.out.push_str("<li>"),
            Tag::Table(_) => self.out.push_str("<table>\n"),
            Tag::TableHead => {
                self.in_table_head = true;
                self.out.push_str("<thead>\n");
            }
            Tag::TableRow => self.out.push_str("<tr>"),
            Tag::TableCell => self
                .out
                .push_str(if self.in_table_head { "<th>" } else { "<td>" }),
            Tag::Emphasis => self.out.push_str("<em>"),
            Tag::Strong => self.out.push_str("<strong>"),
            Tag::Strikethrough => self.out.push_str("<del>"),
            Tag::Link { dest_url, .. } => match safe_href(&dest_url) {
                Some(href) => {
                    self.out.push_str("<a href=\"");
                    escape_into(&mut self.out, &href);
                    self.out.push('"');
                    if is_absolute_web(&href) {
                        self.out
                            .push_str(" target=\"_blank\" rel=\"noopener noreferrer\"");
                    }
                    self.out.push('>');
                    self.open_links.push(true);
                }
                // Unsafe scheme: drop the anchor, keep the label text.
                None => self.open_links.push(false),
            },
            // Images are reduced to their alt text (already plain `Text`).
            Tag::Image { .. } => self.open_links.push(false),
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.out.push_str("</p>\n"),
            TagEnd::Heading(level) => {
                self.out.push_str("</");
                self.out.push_str(heading_tag(level));
                self.out.push_str(">\n");
            }
            TagEnd::BlockQuote(_) => self.out.push_str("</blockquote>\n"),
            TagEnd::CodeBlock => self.out.push_str("</code></pre>\n"),
            TagEnd::HtmlBlock => {}
            TagEnd::List(ordered) => self
                .out
                .push_str(if ordered { "</ol>\n" } else { "</ul>\n" }),
            TagEnd::Item => self.out.push_str("</li>\n"),
            TagEnd::Table => self.out.push_str("</table>\n"),
            TagEnd::TableHead => {
                self.in_table_head = false;
                self.out.push_str("</thead>\n");
            }
            TagEnd::TableRow => self.out.push_str("</tr>\n"),
            TagEnd::TableCell => {
                self.out
                    .push_str(if self.in_table_head { "</th>" } else { "</td>" })
            }
            TagEnd::Emphasis => self.out.push_str("</em>"),
            TagEnd::Strong => self.out.push_str("</strong>"),
            TagEnd::Strikethrough => self.out.push_str("</del>"),
            TagEnd::Link => {
                if self.open_links.pop().unwrap_or(false) {
                    self.out.push_str("</a>");
                }
            }
            TagEnd::Image => {
                self.open_links.pop();
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn finish(self) -> String {
        self.out
    }
}

fn heading_tag(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "h1",
        HeadingLevel::H2 => "h2",
        HeadingLevel::H3 => "h3",
        HeadingLevel::H4 => "h4",
        HeadingLevel::H5 => "h5",
        HeadingLevel::H6 => "h6",
    }
}

/// Whether a link destination is safe to emit. Schemes other than
/// `http`/`https`/`mailto` (e.g. `javascript:`, `data:`) are rejected; plain
/// relative links and fragments are allowed.
fn safe_href(href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return None;
    }
    match scheme(trimmed) {
        Some(scheme) if !matches!(scheme.as_str(), "http" | "https" | "mailto") => None,
        _ => Some(trimmed.to_owned()),
    }
}

fn is_absolute_web(href: &str) -> bool {
    matches!(scheme(href).as_deref(), Some("http" | "https"))
}

/// The scheme of a URL, if it has one (`javascript:...` → `javascript`).
fn scheme(href: &str) -> Option<String> {
    let colon = href.find(':')?;
    let (before, _) = href.split_at(colon);
    // `foo/bar:baz` is a path, not a scheme: schemes have no `/`, `?` or `#`.
    if before.is_empty()
        || !before
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some(before.to_ascii_lowercase())
}

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_lists_and_code_render() {
        let html = render("# Title\n\n- one\n- two\n\n```rust\nlet x = 1;\n```\n");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<ul>"));
        assert!(html.contains("<li>one</li>"));
        assert!(html.contains("<pre><code class=\"language-rust\">"));
        assert!(html.contains("let x = 1;"));
    }

    #[test]
    fn tables_and_emphasis_render() {
        let html = render("| a | b |\n| - | - |\n| 1 | **2** |\n");
        assert!(html.contains("<table>"));
        assert!(html.contains("<th>a</th>"));
        assert!(html.contains("<td>1</td>"));
        assert!(html.contains("<strong>2</strong>"));
    }

    #[test]
    fn links_render_and_external_links_are_hardened() {
        let html = render("[site](https://example.com) and [local](/help)");
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains("target=\"_blank\" rel=\"noopener noreferrer\""));
        assert!(html.contains("href=\"/help\""));
        assert!(!html.contains("href=\"/help\" target"));
    }

    #[test]
    fn raw_html_is_dropped() {
        let html = render("<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n\nplain");
        assert!(!html.contains("<script"), "script survived: {html}");
        assert!(!html.contains("onerror"), "onerror survived: {html}");
        assert!(!html.contains("<img"), "img survived: {html}");
        assert!(html.contains("plain"));
    }

    #[test]
    fn dangerous_link_schemes_are_inert() {
        let html = render("[x](javascript:alert(1)) [y](data:text/html,<script>)");
        assert!(!html.to_lowercase().contains("javascript:"), "{html}");
        assert!(!html.to_lowercase().contains("data:"), "{html}");
        // The label text is kept, just without an anchor.
        assert!(html.contains("x"));
        assert!(!html.contains("<a "));
    }

    #[test]
    fn text_is_escaped() {
        let html = render("5 < 6 & \"quotes\"");
        assert!(html.contains("5 &lt; 6 &amp; &quot;quotes&quot;"));
    }

    #[test]
    fn images_become_alt_text() {
        let html = render("![a diagram](https://example.com/x.png)");
        assert!(!html.contains("<img"));
        assert!(html.contains("a diagram"));
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert_eq!(render(""), "");
    }
}
