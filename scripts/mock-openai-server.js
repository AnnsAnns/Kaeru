// Mock OpenAI-compatible server for local end-to-end verification of
// agent-web's live HTTP client path (not part of the app; dev tooling).
const http = require("http");

const KEY = "mock-key";
const MODEL = "mock-model";

const server = http.createServer((req, res) => {
  const url = new URL(req.url, "http://localhost");
  const auth = req.headers["authorization"] || "";

  if (url.pathname === "/v1/models") {
    if (auth !== `Bearer ${KEY}`) {
      res.writeHead(401, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: "bad key for models" } }));
      return;
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ data: [{ id: MODEL }, { id: "other-model" }] }));
    return;
  }

  if (url.pathname === "/v1/chat/completions" && req.method === "POST") {
    if (auth !== `Bearer ${KEY}`) {
      res.writeHead(401, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: "bad key for chat" } }));
      return;
    }
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const payload = JSON.parse(body);
      const sawStreamOption =
        payload.stream_options && payload.stream_options.include_usage === true;
      console.log(
        `[mock] chat model=${payload.model} messages=${payload.messages.length} stream=${payload.stream} include_usage=${sawStreamOption}`
      );
      res.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
      });
      const chunks = [
        { delta: { role: "assistant" } },
        { delta: { content: "Hello" } },
        { delta: { content: " from" } },
        { delta: { content: " the mock server!" } },
        { delta: {}, finish_reason: "stop" },
      ];
      let i = 0;
      const timer = setInterval(() => {
        if (i < chunks.length) {
          res.write(`data: ${JSON.stringify({ id: "mock", choices: [chunks[i]] })}\n\n`);
          i += 1;
        } else {
          res.write(
            `data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 7, completion_tokens: 9, total_tokens: 16 } })}\n\n`
          );
          res.write("data: [DONE]\n\n");
          clearInterval(timer);
          res.end();
        }
      }, 40);
    });
    return;
  }

  res.writeHead(404, { "content-type": "application/json" });
  res.end(JSON.stringify({ error: { message: `no route ${req.method} ${url.pathname}` } }));
});

server.listen(9911, "127.0.0.1", () => console.log("[mock] listening on 127.0.0.1:9911"));
