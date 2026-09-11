// Sanitization fixture test (M2.5 acceptance, ADR-025): model output is
// untrusted, so raw HTML and dangerous link schemes must render inert while
// ordinary formatting still works. Runnable with `npm test` (plain Node).

import { test } from "node:test";
import assert from "node:assert/strict";

import { renderMarkdown } from "../src/lib/markdown.js";

test("raw HTML and event handlers are dropped", () => {
  const html = renderMarkdown(
    '<script>alert(1)</script>\n\n<img src="x" onerror="alert(1)">\n\n<b>bold</b>',
  );
  assert.ok(!html.includes("<script"), `script survived: ${html}`);
  assert.ok(!html.includes("onerror"), `onerror survived: ${html}`);
  assert.ok(!html.includes("<img"), `img survived: ${html}`);
  // Raw <b> is stripped but its text survives.
  assert.ok(html.includes("bold"));
});

test("javascript: links render inert", () => {
  const html = renderMarkdown("[click](javascript:alert(1))");
  assert.ok(!/javascript:/i.test(html), `javascript link survived: ${html}`);
});

test("headings, lists, code, tables and links render", () => {
  const html = renderMarkdown(
    [
      "# Title",
      "",
      "- one",
      "- two",
      "",
      "```rust",
      "let x = 1;",
      "```",
      "",
      "| a | b |",
      "| - | - |",
      "| 1 | 2 |",
      "",
      "[site](https://example.com)",
    ].join("\n"),
  );
  assert.ok(html.includes("<h1"));
  assert.ok(html.includes("<ul"));
  assert.ok(html.includes("<code"));
  assert.ok(html.includes("<table"));
  assert.ok(html.includes('href="https://example.com"'));
  // External links are hardened.
  assert.ok(html.includes('rel="noopener noreferrer"'));
  assert.ok(html.includes('target="_blank"'));
});

test("empty and undefined input are safe", () => {
  assert.equal(renderMarkdown(""), "");
  assert.equal(renderMarkdown(undefined), "");
});
