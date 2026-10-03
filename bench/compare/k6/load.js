// One k6 run against one server. Discovers the whole HLS presentation from the master playlist
// (variant and audio-rendition playlists, init segments, media segments), so the same script
// drives both servers despite their different URL schemes.
//
//   MASTER    absolute URL of the master playlist
//   SCENARIO  "manifests" (master and media playlists) or "segments" (init and media segments)
//   VUS, DURATION, NAME (summary file name under /results)
//   ENCODING  Accept-Encoding sent with each request (default: what browsers send)
//
// Response bodies are discarded under load: parsing them made k6, not the server, the limit.
// Bytes on the wire are still counted, compressed if the server compressed them.
import http from 'k6/http';
import { check } from 'k6';

const MASTER = __ENV.MASTER;
const SCENARIO = __ENV.SCENARIO || 'segments';
const HEADERS = { 'Accept-Encoding': __ENV.ENCODING || 'gzip, deflate, br' };

export const options = {
  vus: Number(__ENV.VUS || 64),
  duration: __ENV.DURATION || '30s',
  discardResponseBodies: true,
  summaryTrendStats: ['avg', 'min', 'med', 'p(90)', 'p(95)', 'p(99)', 'max'],
  setupTimeout: '120s',
};

function resolve(base, reference) {
  if (/^https?:\/\//.test(reference)) return reference;
  const origin = base.match(/^https?:\/\/[^/]+/)[0];
  if (reference.startsWith('/')) return origin + reference;
  const path = base.split('?')[0];
  return path.slice(0, path.lastIndexOf('/') + 1) + reference;
}

function attribute(line, name) {
  const match = line.match(new RegExp(`${name}="([^"]*)"`));
  return match ? match[1] : null;
}

function get(url) {
  const response = http.get(url, { responseType: 'text' });
  if (response.status !== 200) throw new Error(`${url}: ${response.status}`);
  return response.body;
}

export function setup() {
  const master = get(MASTER);
  const lines = master.split('\n').map((line) => line.trim());
  const playlists = [];
  lines.forEach((line, index) => {
    if (line.startsWith('#EXT-X-STREAM-INF')) playlists.push(resolve(MASTER, lines[index + 1]));
    if (line.startsWith('#EXT-X-MEDIA') && attribute(line, 'URI')) {
      playlists.push(resolve(MASTER, attribute(line, 'URI')));
    }
  });
  const segments = [];
  for (const playlist of playlists) {
    for (const line of get(playlist).split('\n').map((l) => l.trim())) {
      if (line.startsWith('#EXT-X-MAP') && attribute(line, 'URI')) {
        segments.push(resolve(playlist, attribute(line, 'URI')));
      } else if (line && !line.startsWith('#')) {
        segments.push(resolve(playlist, line));
      }
    }
  }
  if (segments.length === 0) throw new Error('no segments discovered');
  return { manifests: [MASTER, ...playlists], segments };
}

export default function (data) {
  const list = SCENARIO === 'manifests' ? data.manifests : data.segments;
  const url = list[Math.floor(Math.random() * list.length)];
  const response = http.get(url, { headers: HEADERS });
  check(response, { 'status 200': (r) => r.status === 200 });
}

export function handleSummary(data) {
  return { [`/results/${__ENV.NAME || 'summary'}.json`]: JSON.stringify(data) };
}
