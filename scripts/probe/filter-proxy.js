"use strict";

const fs = require("fs");
const http = require("http");
const net = require("net");

const [portFile, logFile, ...allowRules] = process.argv.slice(2);
if (!portFile || !logFile) {
  process.stderr.write("usage: filter-proxy.js PORT_FILE LOG_FILE [HOST[:PORT]|*.example.com[:PORT] ...]\n");
  process.exit(2);
}

const rules = allowRules.map((rule) => {
  const m = /^(.*?)(?::(\d+))?$/.exec(rule);
  return { host: m[1].toLowerCase(), port: m[2] ? Number(m[2]) : null };
});

const upstreamUrl = process.env.HTTPS_PROXY || process.env.https_proxy || "";
const upstream = upstreamUrl ? new URL(upstreamUrl) : null;
const upstreamAuth =
  upstream && upstream.username
    ? "Basic " + Buffer.from(decodeURIComponent(upstream.username) + ":" + decodeURIComponent(upstream.password)).toString("base64")
    : null;

function hostMatches(ruleHost, host) {
  if (ruleHost.startsWith("*.")) {
    const suffix = ruleHost.slice(1);
    return host.endsWith(suffix) && host.length > suffix.length;
  }
  return ruleHost === host;
}

function isAllowed(host, port) {
  return rules.some((r) => hostMatches(r.host, host) && (r.port === null ? port === 443 || port === 80 : r.port === port));
}

const seen = new Set();
function logDecision(host, port, decision) {
  const key = `${host}:${port}:${decision}`;
  if (seen.has(key)) return;
  seen.add(key);
  fs.appendFileSync(logFile, `${new Date().toISOString()} ${decision} ${host}:${port}\n`);
}

function parseTarget(hostport, defaultPort) {
  const m = /^\[?([^\]]*?)\]?(?::(\d+))?$/.exec(hostport || "");
  if (!m || !m[1]) return null;
  return { host: m[1].toLowerCase(), port: m[2] ? Number(m[2]) : defaultPort };
}

function connectUpstream(host, port, onReady, onError) {
  const socket = net.connect(Number(upstream.port) || (upstream.protocol === "https:" ? 443 : 80), upstream.hostname);
  let header = "";
  let settled = false;
  socket.once("error", (e) => {
    if (!settled) {
      settled = true;
      onError(e);
    }
  });
  socket.on("connect", () => {
    socket.write(
      `CONNECT ${host}:${port} HTTP/1.1\r\nHost: ${host}:${port}\r\n` +
        (upstreamAuth ? `Proxy-Authorization: ${upstreamAuth}\r\n` : "") +
        "\r\n"
    );
  });
  const onData = (chunk) => {
    header += chunk.toString("latin1");
    const end = header.indexOf("\r\n\r\n");
    if (end === -1) return;
    socket.removeListener("data", onData);
    const status = /^HTTP\/1\.[01] (\d{3})/.exec(header);
    if (!status || status[1] !== "200") {
      settled = true;
      socket.destroy();
      onError(new Error(`upstream proxy answered ${status ? status[1] : "garbage"}`));
      return;
    }
    settled = true;
    const rest = Buffer.from(header.slice(end + 4), "latin1");
    onReady(socket, rest);
  };
  socket.on("data", onData);
}

function connectTarget(host, port, onReady, onError) {
  if (upstream) {
    connectUpstream(host, port, onReady, onError);
    return;
  }
  const socket = net.connect(port, host);
  socket.once("error", onError);
  socket.once("connect", () => onReady(socket, Buffer.alloc(0)));
}

const server = http.createServer((req, res) => {
  let target;
  try {
    target = new URL(req.url);
  } catch (e) {
    res.writeHead(400);
    res.end("filter-proxy: absolute URL required\n");
    return;
  }
  const host = target.hostname.toLowerCase();
  const port = Number(target.port) || 80;
  if (!isAllowed(host, port)) {
    logDecision(host, port, "deny");
    res.writeHead(403);
    res.end(`filter-proxy: ${host}:${port} not allowed\n`);
    return;
  }
  logDecision(host, port, "allow");
  const headers = { ...req.headers };
  delete headers["proxy-connection"];
  let options;
  if (upstream) {
    if (upstreamAuth) headers["proxy-authorization"] = upstreamAuth;
    options = { host: upstream.hostname, port: Number(upstream.port) || 80, method: req.method, path: req.url, headers };
  } else {
    options = { host, port, method: req.method, path: target.pathname + target.search, headers };
  }
  const proxyReq = http.request(options, (proxyRes) => {
    res.writeHead(proxyRes.statusCode, proxyRes.headers);
    proxyRes.pipe(res);
  });
  proxyReq.on("error", (e) => {
    if (!res.headersSent) res.writeHead(502);
    res.end(`filter-proxy: ${e.message}\n`);
  });
  req.pipe(proxyReq);
});

server.on("connect", (req, clientSocket, head) => {
  const target = parseTarget(req.url, 443);
  if (!target) {
    clientSocket.end("HTTP/1.1 400 Bad Request\r\n\r\n");
    return;
  }
  const { host, port } = target;
  if (!isAllowed(host, port)) {
    logDecision(host, port, "deny");
    clientSocket.end("HTTP/1.1 403 Forbidden\r\n\r\n");
    return;
  }
  logDecision(host, port, "allow");
  clientSocket.on("error", () => {});
  connectTarget(
    host,
    port,
    (serverSocket, rest) => {
      serverSocket.on("error", () => clientSocket.destroy());
      clientSocket.on("error", () => serverSocket.destroy());
      clientSocket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      if (rest.length) clientSocket.write(rest);
      if (head && head.length) serverSocket.write(head);
      serverSocket.pipe(clientSocket);
      clientSocket.pipe(serverSocket);
    },
    (e) => {
      clientSocket.end(`HTTP/1.1 502 Bad Gateway\r\n\r\nfilter-proxy: ${e.message}\n`);
    }
  );
});

server.on("clientError", (e, socket) => {
  if (socket.writable) socket.end("HTTP/1.1 400 Bad Request\r\n\r\n");
});

server.listen(0, "127.0.0.1", () => {
  const port = server.address().port;
  const tmp = `${portFile}.tmp`;
  fs.writeFileSync(tmp, `${port}\n`);
  fs.renameSync(tmp, portFile);
});

process.on("SIGTERM", () => process.exit(0));
process.on("SIGINT", () => process.exit(0));
