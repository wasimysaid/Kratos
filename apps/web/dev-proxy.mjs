import http from "node:http";
import net from "node:net";

const edgePath = /^\/(?:api|auth|device|session|tail|stats|diff|snapshot|append|chat2|workspace|registry|blob|preview|health)(?:\/|$)/;
const target = (url) => edgePath.test(url) ? 27640 : 3001;

const server = http.createServer((request, response) => {
  const port = target(request.url);
  const proxy = http.request(
    { host: "127.0.0.1", port, method: request.method, path: request.url, headers: { ...request.headers, host: `127.0.0.1:${port}` } },
    (upstream) => {
      response.writeHead(upstream.statusCode ?? 502, upstream.headers);
      upstream.pipe(response);
    }
  );
  proxy.on("error", () => response.writeHead(502).end("development upstream unavailable"));
  request.pipe(proxy);
});

server.on("upgrade", (request, socket, head) => {
  const port = target(request.url);
  const upstream = net.connect(port, "127.0.0.1");
  upstream.on("connect", () => {
    upstream.write(`${request.method} ${request.url} HTTP/${request.httpVersion}\r\nHost: 127.0.0.1:${port}\r\n`);
    for (let index = 0; index < request.rawHeaders.length; index += 2) {
      if (request.rawHeaders[index].toLowerCase() !== "host") upstream.write(`${request.rawHeaders[index]}: ${request.rawHeaders[index + 1]}\r\n`);
    }
    upstream.write("\r\n");
    if (head.length) upstream.write(head);
    socket.pipe(upstream).pipe(socket);
  });
  upstream.on("error", () => socket.destroy());
});

server.listen(3000, "0.0.0.0");
