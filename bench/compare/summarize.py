"""Turns one run.sh results directory into a Markdown report on stdout."""
import csv
import json
import os
import platform
import statistics
import sys

out = sys.argv[1]
servers = [s for s in ("segmentor", "nginx", "nginx-cached")
           if os.path.exists(f"{out}/{s}-cold.csv")]
label = {"segmentor": "segmentor", "nginx": "nginx-vod-module",
         "nginx-cached": "nginx-vod-module, response cache"}


def k6(server, scenario):
    with open(f"{out}/{server}-{scenario}.json") as handle:
        metrics = json.load(handle)["metrics"]
    duration = metrics["http_req_duration"]["values"]
    requests = metrics["http_reqs"]["values"]
    received = metrics["data_received"]["values"]
    failed = metrics.get("http_req_failed", {}).get("values", {}).get("rate", 0.0)
    return {
        "rps": requests["rate"],
        "mib_s": received["rate"] / 1024 / 1024,
        "p50": duration["med"],
        "p95": duration["p(95)"],
        "p99": duration["p(99)"],
        "failed": failed * 100,
    }


def stats(server, scenario):
    rows = list(csv.DictReader(open(f"{out}/{server}-{scenario}-stats.csv")))
    if not rows:
        return None
    cpu = [float(row["cpu_percent"]) for row in rows]
    mem = [int(row["mem_bytes"]) for row in rows]
    return {"cpu": statistics.mean(cpu), "mem_mib": max(mem) / 1024 / 1024}


def cold(server):
    rows = list(csv.DictReader(open(f"{out}/{server}-cold.csv")))
    pick = lambda key: statistics.median(float(row[key]) for row in rows) * 1000
    return {key: pick(f"{key}_s") for key in ("master", "playlist", "segment")}, len(rows)


def read(path, default="missing"):
    try:
        return open(path).read().strip()
    except OSError:
        return default


print(f"# segmentor vs nginx-vod-module: {os.path.basename(out)}\n")
print(f"Host: {platform.machine()}, {os.cpu_count()} logical CPUs. Servers pinned to "
      f"`{os.environ.get('SERVER_CPUS', '0-3')}`, load generator to "
      f"`{os.environ.get('LOAD_CPUS', '4-7')}`.\n")

print("## Correctness\n")
print("| Server | FFmpeg decode (first 30 s) | Playlists | Segments | Requests | Payload | Duration |")
print("| --- | --- | --- | --- | --- | --- | --- |")
for server in servers:
    crawl = json.loads(read(f"{out}/{server}-crawl.json", "{}") or "{}")
    print(f"| {label[server]} | {read(f'{out}/{server}-decode-status')} | {crawl.get('playlists', '?')} "
          f"| {crawl.get('segments', '?')} | {crawl.get('requests', '?')} "
          f"| {crawl.get('payload_bytes', 0) / 1024 / 1024:.1f} MiB | {crawl.get('duration_s', '?')} s |")

print("\n## Cold start (median of fresh processes, ms)\n")
print("| Server | First master playlist | Then media playlist | Then first segment |")
print("| --- | ---: | ---: | ---: |")
for server in servers:
    values, reps = cold(server)
    print(f"| {label[server]} ({reps} runs) | {values['master']:.1f} | {values['playlist']:.1f} "
          f"| {values['segment']:.1f} |")

for scenario in ("manifests", "segments"):
    print(f"\n## Under load: {scenario}\n")
    print("| Server | Requests/s | MiB/s | p50 ms | p95 ms | p99 ms | Failed | Server CPU % | Peak RSS MiB |")
    print("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for server in servers:
        result = k6(server, scenario)
        usage = stats(server, scenario) or {"cpu": float("nan"), "mem_mib": float("nan")}
        print(f"| {label[server]} | {result['rps']:.0f} | {result['mib_s']:.1f} | {result['p50']:.2f} "
              f"| {result['p95']:.2f} | {result['p99']:.2f} | {result['failed']:.2f} % "
              f"| {usage['cpu']:.0f} | {usage['mem_mib']:.0f} |")

print("\n## Efficiency\n")
print("| Server | Playlist sets/s | Playlist requests/s per core | Segment MiB/s per core |")
print("| --- | ---: | ---: | ---: |")
for server in servers:
    crawl = json.loads(read(f"{out}/{server}-crawl.json", "{}") or "{}")
    manifests = k6(server, "manifests")
    # One viewer fetches the master and every playlist it lists; k6 picks among them uniformly.
    per_set = crawl.get("playlists", 0) + 1
    sets = f"{manifests['rps'] / per_set:.0f}" if crawl else "?"
    cells = [sets]
    for scenario, key in (("manifests", "rps"), ("segments", "mib_s")):
        cpu = (stats(server, scenario) or {}).get("cpu")
        cells.append(f"{k6(server, scenario)[key] / (cpu / 100):.0f}" if cpu else "?")
    print(f"| {label[server]} | " + " | ".join(cells) + " |")
print("\nCPU % is `docker stats`, where 100 % is one core. Requests/s are not comparable on their "
      "own when the servers split the presentation into different numbers of requests: compare "
      "playlist sets/s (a viewer's master and every playlist it lists) and segment MiB/s. "
      "Manifest MiB/s are bytes on the wire, compressed when the server compressed them.")
