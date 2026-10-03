"""Fetches a whole HLS presentation once and reports what it is made of, as JSON.

Used by run.sh to check both servers serve a comparable presentation: the same duration, and
roughly the same payload, however they split it into requests (nginx-vod-module muxes audio into
the video segments by default; segmentor serves audio as its own rendition).
"""
import json
import sys
import urllib.request
from urllib.parse import urljoin


def fetch(url):
    with urllib.request.urlopen(url) as response:
        return response.read()


def attribute(line, name):
    marker = f'{name}="'
    if marker not in line:
        return None
    return line.split(marker, 1)[1].split('"', 1)[0]


master_url = sys.argv[1]
lines = [line.strip() for line in fetch(master_url).decode().splitlines()]
playlists = []
for index, line in enumerate(lines):
    if line.startswith("#EXT-X-STREAM-INF"):
        playlists.append(urljoin(master_url, lines[index + 1]))
    if line.startswith("#EXT-X-MEDIA") and attribute(line, "URI"):
        playlists.append(urljoin(master_url, attribute(line, "URI")))

requests = 0
payload = 0
duration = 0.0
segments = 0
for playlist in playlists:
    body = fetch(playlist).decode()
    playlist_duration = 0.0
    for line in (l.strip() for l in body.splitlines()):
        url = None
        if line.startswith("#EXT-X-MAP") and attribute(line, "URI"):
            url = urljoin(playlist, attribute(line, "URI"))
        elif line.startswith("#EXTINF:"):
            playlist_duration += float(line[len("#EXTINF:"):].split(",")[0])
        elif line and not line.startswith("#"):
            url = urljoin(playlist, line)
            segments += 1
        if url:
            payload += len(fetch(url))
            requests += 1
    duration = max(duration, playlist_duration)

print(json.dumps({
    "playlists": len(playlists),
    "segments": segments,
    "requests": requests,
    "payload_bytes": payload,
    "duration_s": round(duration, 3),
}))
