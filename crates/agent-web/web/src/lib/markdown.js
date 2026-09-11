// Client-side Markdown rendering (M2.5, ADR-025 / §6.8).
//
// Assistant replies are raw Markdown on the wire; this module turns them into
// **sanitized HTML** with the unified pipeline:
//   remark-parse -> remark-gfm -> remark-rehype -> rehype-sanitize -> stringify
//
// Sanitization happens on the Hast tree (structurally), before serialization,
// so raw HTML and dangerous link schemes (javascript:) are dropped. `agent-core`
// and the API only ever see raw text (C2).

import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkGfm from "remark-gfm";
import remarkRehype from "remark-rehype";
import rehypeSanitize, { defaultSchema } from "rehype-sanitize";
import rehypeStringify from "rehype-stringify";

// Harden links *after* sanitization: external http(s) links open in a new tab
// and carry rel="noopener noreferrer". Numbers only, so it stays inert.
function rehypeLinkSafety() {
  return (tree) => {
    const walk = (node) => {
      if (node.type === "element" && node.tagName === "a") {
        const href = node.properties?.href;
        if (typeof href === "string" && /^https?:/i.test(href)) {
          node.properties.rel = ["noopener", "noreferrer"];
          node.properties.target = "_blank";
        }
      }
      for (const child of node.children || []) walk(child);
    };
    walk(tree);
  };
}

const processor = unified()
  .use(remarkParse)
  .use(remarkGfm)
  .use(remarkRehype)
  .use(rehypeSanitize, defaultSchema)
  .use(rehypeLinkSafety)
  .use(rehypeStringify);

/** Render raw Markdown to a sanitized HTML string. Never throws on bad input. */
export function renderMarkdown(markdown) {
  try {
    return String(processor.processSync(markdown ?? ""));
  } catch (err) {
    if (typeof console !== "undefined") {
      console.warn("markdown render failed; showing plain text", err);
    }
    return "";
  }
}
