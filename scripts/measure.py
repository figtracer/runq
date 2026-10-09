#!/usr/bin/env python3
"""Measure the release binary. Workloads sleep or exit; no stress allocation."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("--binary", default="target/release/runq")
parser.add_argument("--output", default="results/measurement.json")
parser.add_argument("--seconds", type=float, default=5)
parser.add_argument("--max-daemon-mib", type=float)
parser.add_argument("--max-supervisor-mib", type=float)
parser.add_argument("--max-binary-kib", type=float)
parser.add_argument("--max-manager-mib", type=float)
args = parser.parse_args()
binary = Path(args.binary).resolve()


def inspect(pid):
    text = subprocess.check_output([str(binary), "inspect", str(pid)], text=True)
    return {k: int(v) for k, v in (part.split("=") for part in text.split())}


def cpu_window(pid):
    before = inspect(pid)
    start = time.monotonic()
    time.sleep(args.seconds)
    after = inspect(pid)
    duration = time.monotonic() - start
    return {"before": before, "after": after, "seconds": duration,
            "cpu_percent_one_core": (after["cpu_ns"] - before["cpu_ns"]) / (duration * 1e9) * 100}


def latency(command, env):
    values = []
    for _ in range(20):
        start = time.monotonic_ns()
        # Pipes let communicate wait on readiness. DEVNULL plus a timeout uses
        # Python's polling wait, which quantizes these short launch measurements.
        subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, timeout=10)
        values.append((time.monotonic_ns() - start) / 1e6)
    return {"samples_ms": values, "median_ms": statistics.median(values),
            "p95_ms": sorted(values)[18]}


with tempfile.TemporaryDirectory(prefix="rq-bench-", dir="/tmp") as directory:
    env = {**os.environ, "RUNQ_SOCKET": directory + "/s"}
    env.pop("RUNQ_JOB_ID", None)
    for key in ("MallocNanoZone", "MallocSpaceEfficient", "MallocMaxMagazines",
                "_RUNQ_ALLOCATOR_READY", "_RUNQ_ORIGINAL_MALLOC_MAX_MAGAZINES",
                "_RUNQ_ORIGINAL_MALLOC_NANO_ZONE"):
        env.pop(key, None)
    children = []
    with open(directory + "/daemon.log", "w+") as log:
        daemon = subprocess.Popen([str(binary), "daemon", "--memory", "128MiB"],
                                  env=env, stdout=log, stderr=log)
        try:
            end = time.monotonic() + 5
            while not Path(env["RUNQ_SOCKET"]).exists():
                if daemon.poll() is not None or time.monotonic() > end:
                    log.seek(0)
                    raise RuntimeError(log.read())
                time.sleep(.01)
            report = {
                "platform": platform.platform(), "machine": platform.machine(),
                "binary_bytes": binary.stat().st_size,
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "scope": "Single local run; macOS memory_bytes is physical footprint; RSS is reported separately. CPU is percent of one core. Workload memory excluded from manager totals. No competitor benchmark.",
                "latency_method": "20 sequential runs with communicate/pipe readiness and a 10-second timeout; includes harness and process startup/cleanup",
                "daemon_idle": cpu_window(daemon.pid),
                "direct_true": latency(["/usr/bin/true"], env),
                "runq_true": latency([str(binary), "exec", "--", "/usr/bin/true"], env),
            }
            children.append(subprocess.Popen([str(binary), "exec", "--", "/bin/sleep", "60"],
                                             env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
            time.sleep(.15)
            report["daemon_one_running"] = cpu_window(daemon.pid)
            report["one_supervisor"] = inspect(children[0].pid)
            for _ in range(7):
                children.append(subprocess.Popen([str(binary), "exec", "--", "/bin/sleep", "60"],
                                                 env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
            time.sleep(.2)
            report["eight_clients"] = {
                "daemon": inspect(daemon.pid),
                "clients": [inspect(p.pid) for p in children],
                "status": subprocess.check_output([str(binary), "status"], env=env, text=True),
            }
            report["eight_clients"]["manager_memory_bytes"] = report["eight_clients"]["daemon"]["memory_bytes"] + sum(p["memory_bytes"] for p in report["eight_clients"]["clients"])
            report["daemon_eight_clients_cpu"] = cpu_window(daemon.pid)
            report["acceptance"] = {}
            if args.max_manager_mib is not None:
                total = report["eight_clients"]["manager_memory_bytes"]
                report["acceptance"]["eight_client_memory"] = {
                    "limit_bytes": int(args.max_manager_mib * 2**20),
                    "measured_bytes": total,
                    "passed": total <= args.max_manager_mib * 2**20}
            if args.max_binary_kib is not None:
                report["acceptance"]["binary_size"] = {
                    "limit_bytes": int(args.max_binary_kib * 1024),
                    "measured_bytes": report["binary_bytes"],
                    "passed": report["binary_bytes"] <= args.max_binary_kib * 1024}
            if args.max_daemon_mib is not None:
                peak = max(report["daemon_idle"]["after"]["memory_bytes"],
                           report["daemon_one_running"]["after"]["memory_bytes"],
                           report["eight_clients"]["daemon"]["memory_bytes"])
                report["acceptance"]["daemon_memory"] = {
                    "limit_bytes": int(args.max_daemon_mib * 2**20), "measured_bytes": peak,
                    "passed": peak <= args.max_daemon_mib * 2**20}
            if args.max_supervisor_mib is not None:
                peak = max(p["memory_bytes"] for p in report["eight_clients"]["clients"])
                report["acceptance"]["supervisor_memory"] = {
                    "limit_bytes": int(args.max_supervisor_mib * 2**20), "measured_bytes": peak,
                    "passed": peak <= args.max_supervisor_mib * 2**20}
            output = Path(args.output)
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps({
                "binary_bytes": report["binary_bytes"],
                "daemon_idle": report["daemon_idle"],
                "daemon_active_cpu_percent": report["daemon_one_running"]["cpu_percent_one_core"],
                "supervisor": report["one_supervisor"],
                "eight_clients_total_memory_bytes": report["eight_clients"]["manager_memory_bytes"],
                "launch_median_ms": report["runq_true"]["median_ms"],
                "launch_p95_ms": report["runq_true"]["p95_ms"],
                "acceptance": report["acceptance"],
            }, indent=2))
            if not all(item["passed"] for item in report["acceptance"].values()):
                raise SystemExit("size or memory regression budget exceeded")
        finally:
            if daemon.poll() is None:
                subprocess.run([str(binary), "shutdown"], env=env, capture_output=True, timeout=5)
            for child in children:
                try:
                    child.wait(timeout=6)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
            try:
                daemon.wait(timeout=6)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
