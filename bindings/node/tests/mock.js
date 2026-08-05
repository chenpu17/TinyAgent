// Configurable OpenAI-compatible SSE mock for the Node binding tests.
// Mirrors the Python tests' ScriptedMock: plays back scripted responses per
// request, captures request bodies for assertions, and supports a "hang"
// responder for the cancel test. Pure Node http — no Python dependency.

const http = require('node:http');

function sseBytes(chunks, done = true) {
  let out = '';
  for (const c of chunks) out += `data: ${JSON.stringify(c)}\n\n`;
  if (done) out += 'data: [DONE]\n\n';
  return Buffer.from(out);
}

/** A streaming tool_call: name first, then arguments, then finish=tool_calls. */
function toolCallSse(id, name, args) {
  return sseBytes([
    { choices: [{ index: 0, finish_reason: null, delta: {
      role: 'assistant',
      tool_calls: [{ index: 0, id, type: 'function', function: { name, arguments: '' } }],
    } }] },
    { choices: [{ index: 0, finish_reason: null, delta: {
      tool_calls: [{ index: 0, function: { arguments: JSON.stringify(args) } }],
    } }] },
    { choices: [{ index: 0, finish_reason: 'tool_calls', delta: {} }] },
  ]);
}

/** A final text answer, optionally carrying a usage frame on the finish chunk. */
function textSse(content, usage = null, finish = 'stop') {
  const chunks = [
    { choices: [{ index: 0, finish_reason: null, delta: { role: 'assistant', content } }] },
    { choices: [{ index: 0, finish_reason: finish, delta: {} }] },
  ];
  if (usage) chunks[1].usage = usage;
  return sseBytes(chunks);
}

function respondSse(res, bytes) {
  res.writeHead(200, {
    'Content-Type': 'text/event-stream',
    'Content-Length': bytes.length,
  });
  res.end(bytes);
}

/** Hold the stream open without data so the agent's cancel poll fires before
 *  the idle/total timeout. Unblocks when the mock is stopped. */
function hang(res, mock) {
  res.writeHead(200, {
    'Content-Type': 'text/event-stream',
    'Content-Length': '9999', // promise more bytes than we send → reader blocks mid-stream
  });
  res.write(': keep-alive\n\n'); // SSE comment, no data frame
  const iv = setInterval(() => {
    if (mock.stopped) {
      clearInterval(iv);
      try { res.end(); } catch { /* already closed */ }
    }
  }, 50);
}

class ScriptedMock {
  constructor(script) {
    this.script = script; // each: (reqBody, res, mock) => void
    this.requests = [];
    this.stopped = false;
    this.server = http.createServer((req, res) => {
      if (req.method !== 'POST' || req.url !== '/chat/completions') {
        res.statusCode = 404;
        res.end();
        return;
      }
      let body = '';
      req.on('data', (d) => (body += d));
      req.on('end', () => {
        const parsed = JSON.parse(body);
        this.requests.push(parsed);
        const idx = this.requests.length - 1;
        const responder = this.script[Math.min(idx, this.script.length - 1)];
        responder(parsed, res, this);
      });
    });
    this.port = 0;
  }

  // Bind synchronously-reliably: Node's server.listen(0) assigns the port
  // asynchronously, so server.address() is null right after listen() returns.
  // Await this so `mock.url` is a real endpoint before the agent connects
  // (a null/garbage port makes reqwest fail to connect — and the agent's
  // transient retry backoff then hangs the test for ages).
  static start(script) {
    return new Promise((resolve, reject) => {
      const m = new ScriptedMock(script);
      m.server.on('error', reject);
      m.server.listen(0, '127.0.0.1', () => {
        m.port = m.server.address().port;
        resolve(m);
      });
    });
  }

  get url() {
    return `http://127.0.0.1:${this.port}`;
  }

  stop() {
    this.stopped = true;
    // Drop in-flight keep-alive connections too, otherwise server.close()
    // waits for them and the test process never exits.
    if (typeof this.server.closeAllConnections === 'function') {
      this.server.closeAllConnections();
    }
    this.server.close();
  }
}

module.exports = { ScriptedMock, toolCallSse, textSse, respondSse, hang };
