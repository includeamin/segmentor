// Starts what the specs play against: the example mapper, the vodnik binary fed by it, and a
// static server for the player page. Their addresses reach the specs through the environment.
import { spawn } from 'node:child_process';
import { createReadStream, existsSync, mkdirSync, openSync, readFileSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../..');
const work = path.join(here, '.harness');

/** Waits until `pattern` matches the file's text, and returns the match. */
async function waitFor(file, pattern, what, child) {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    const text = existsSync(file) ? readFileSync(file, 'utf8') : '';
    const match = text.match(pattern);
    if (match) return match;
    if (child.exitCode !== null) {
      throw new Error(`${what} exited with ${child.exitCode} before it was ready:\n${text}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`${what} was not ready in 30 s:\n${existsSync(file) ? readFileSync(file, 'utf8') : ''}`);
}

function start(command, args, options, logFile) {
  const log = openSync(logFile, 'w');
  return spawn(command, args, { ...options, stdio: ['ignore', log, log] });
}

/** Serves the page, and the two players from node_modules, over loopback. */
function staticServer() {
  const files = {
    '/': ['page/player.html', 'text/html'],
    '/player.js': ['page/player.js', 'text/javascript'],
    '/vendor/hls.js': ['node_modules/hls.js/dist/hls.min.js', 'text/javascript'],
    '/vendor/dash.js': ['node_modules/dashjs/dist/modern/umd/dash.all.min.js', 'text/javascript'],
  };
  const server = http.createServer((request, response) => {
    const entry = files[new URL(request.url, 'http://x').pathname];
    if (!entry) {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, { 'Content-Type': entry[1], 'Cache-Control': 'no-store' });
    createReadStream(path.join(here, entry[0])).pipe(response);
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server)));
}

export default async function globalSetup() {
  const binary = process.env.VODNIK_BIN ?? path.join(root, 'target/debug/vodnik');
  if (!existsSync(binary)) {
    throw new Error(`${binary} does not exist. Build it first: cargo build --locked --bin vodnik (make browser-test does).`);
  }
  mkdirSync(work, { recursive: true });
  const children = [];

  const mapperLog = path.join(work, 'mapper.log');
  const mapper = start(
    'python3',
    [path.join(root, 'examples/mapper/mapper.py')],
    {
      env: {
        ...process.env,
        PORT: '0',
        // HARNESS_CATALOG points the mapper at another catalog, to try an asset out.
        CATALOG_PATH: process.env.HARNESS_CATALOG ?? path.join(root, 'tests/fixtures/harness-catalog.json'),
      },
    },
    mapperLog,
  );
  children.push(mapper);
  const [, mapperPort] = await waitFor(mapperLog, /mapper: serving .* on [^\s:]+:(\d+)/, 'the mapper', mapper);

  const config = [
    '[server]',
    'listen = "127.0.0.1:0"',
    '[storage]',
    `media_root = "${path.join(root, 'tests/fixtures')}"`,
    '[packaging]',
    'segment_duration_ms = 1000',
    '[logging]',
    'level = "info"',
    'format = "compact"',
    // The page and the server are on different loopback ports, so the browser needs CORS.
    '[cors]',
    'enabled = true',
    'allowed_origins = ["*"]',
    '[resolver]',
    'type = "http"',
    '[resolver.http]',
    `base_url = "http://127.0.0.1:${mapperPort}"`,
    'allow_insecure_mapper = true',
    '',
  ].join('\n');
  const configPath = path.join(work, 'vod.toml');
  writeFileSync(configPath, config);
  const serverLog = path.join(work, 'server.log');
  const server = start(binary, ['serve', '--config', configPath], {}, serverLog);
  children.push(server);
  const [, address] = await waitFor(serverLog, /service_ready.* listen\.address=(\S+)/, 'the server', server);

  const pages = await staticServer();
  process.env.VODNIK_URL = `http://${address}`;
  process.env.PAGE_URL = `http://127.0.0.1:${pages.address().port}`;

  return async () => {
    pages.close();
    for (const child of children) child.kill();
  };
}
