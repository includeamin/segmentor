"""Appends one `cpu_percent,mem_bytes` row per second for a container until killed.

docker stats reports CPU where 100 % is one core, and memory like "123.4MiB / 15.4GiB".
"""
import subprocess
import sys
import time

container, path = sys.argv[1], sys.argv[2]
units = {"KiB": 1024, "MiB": 1024**2, "GiB": 1024**3, "kB": 1000, "MB": 1000**2, "GB": 1000**3, "B": 1}
while True:
    line = subprocess.run(
        ["docker", "stats", "--no-stream", "--format", "{{.CPUPerc}},{{.MemUsage}}", container],
        capture_output=True, text=True,
    ).stdout.strip()
    if line:
        cpu, memory = line.split(",", 1)
        used = memory.split("/")[0].strip()
        for unit in sorted(units, key=len, reverse=True):
            if used.endswith(unit):
                with open(path, "a") as out:
                    out.write(f"{cpu.rstrip('%')},{int(float(used[: -len(unit)]) * units[unit])}\n")
                break
    time.sleep(0.5)
