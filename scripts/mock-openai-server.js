// Mock OpenAI-compatible server for local end-to-end verification of
// agent-web's live HTTP client path (not part of the app; dev tooling).
const http = require("http");

const KEY = "mock-key";
const MODEL = "mock-model";
// MOCK_TOOL=python scripts a sandboxed Python call (M5 manual verification);
// the default scripts the M3 web_search call.
const TOOL = process.env.MOCK_TOOL || "web_search";

// Pure-stdlib "plot": reads the uploaded CSV from the workspace and writes a
// tiny bar image, so the full upload -> script -> artifact -> download path is
// exercised without installing matplotlib.
const PYTHON_SCRIPT = `import csv, struct, zlib

with open("data.csv", newline="") as f:
    rows = list(csv.DictReader(f))
total = sum(float(row["amount"]) for row in rows)

def chunk(tag, data):
    return (struct.pack(">I", len(data)) + tag + data +
            struct.pack(">I", zlib.crc32(tag + data) & 0xffffffff))

w = h = 8
bar = max(1, min(h, int(total)))
raw = b"".join(b"\\x00" + bytes([200, 80, 40, 255]) * w for _ in range(bar))
raw += b"".join(b"\\x00" + bytes([30, 30, 60, 255]) * w for _ in range(h - bar))
png = (b"\\x89PNG\\r\\n\\x1a\\n"
       + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(raw))
       + chunk(b"IEND", b""))
open("total.png", "wb").write(png)
print(f"cleaned {len(rows)} rows; total={total}")
`;

function toolCallChunks() {
  if (TOOL === "python-copy") {
    // A dependency-free "do something with my upload": copy the uploaded
    // image, so the artifact round-trip (event + reload) is verifiable.
    const script = 'import shutil\nshutil.copy("photo.png", "photo-copy.png")\nprint("copied")';
    return [
      {
        delta: {
          tool_calls: [
            {
              index: 0,
              id: "call_mock_1",
              type: "function",
              function: { name: "python", arguments: "" },
            },
          ],
        },
      },
      {
        delta: {
          tool_calls: [
            {
              index: 0,
              function: {
                arguments: JSON.stringify({ script }),
              },
            },
          ],
        },
      },
      { delta: {}, finish_reason: "tool_calls" },
    ];
  }
  if (TOOL === "python-deps") {
    const script = 'import cowsay\nprint(cowsay.get_output_string("cow", "kaeru"))\n';
    return [
      {
        delta: {
          tool_calls: [
            {
              index: 0,
              id: "call_mock_1",
              type: "function",
              function: { name: "python", arguments: "" },
            },
          ],
        },
      },
      {
        delta: {
          tool_calls: [
            {
              index: 0,
              function: {
                arguments: JSON.stringify({ script, deps: ["cowsay"] }),
              },
            },
          ],
        },
      },
      { delta: {}, finish_reason: "tool_calls" },
    ];
  }
  if (TOOL === "python") {
    return [
      {
        delta: {
          tool_calls: [
            {
              index: 0,
              id: "call_mock_1",
              type: "function",
              function: { name: "python", arguments: "" },
            },
          ],
        },
      },
      {
        delta: {
          tool_calls: [
            {
              index: 0,
              function: { arguments: JSON.stringify({ script: PYTHON_SCRIPT }) },
            },
          ],
        },
      },
      { delta: {}, finish_reason: "tool_calls" },
    ];
  }
  return [
    {
      delta: {
        tool_calls: [
          {
            index: 0,
            id: "call_mock_1",
            type: "function",
            function: { name: "web_search", arguments: "" },
          },
        ],
      },
    },
    {
      delta: {
        tool_calls: [
          {
            index: 0,
            function: { arguments: '{"query":"kaeru frog agent"}' },
          },
        ],
      },
    },
    { delta: {}, finish_reason: "tool_calls" },
  ];
}

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
      const hasTools = Array.isArray(payload.tools) && payload.tools.length > 0;
      const sawToolResult = payload.messages.some((m) => m.role === "tool");
      console.log(
        `[mock] chat model=${payload.model} messages=${payload.messages.length} stream=${payload.stream} include_usage=${sawStreamOption} tools=${hasTools} sawToolResult=${sawToolResult}`
      );
      res.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
      });

      // Exercise the agent loop: the first turn asks for a tool (web_search,
      // or the sandboxed python script with MOCK_TOOL=python), the turn after
      // the tool result answers in plain text.
      const chunks = hasTools && !sawToolResult
        ? [{ delta: { role: "assistant" } }, ...toolCallChunks()]
        : [
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
