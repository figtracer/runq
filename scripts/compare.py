#!/usr/bin/env python3
"""macOS physical-footprint comparison. Supply isolated competitor executables.

No downloads, installs, real coding agents, or global configuration changes.
Each trial runs alone, with one sleeping workload and seven waiting jobs.
"""
import argparse
from contextlib import ExitStack
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import signal
import socket
import statistics
import struct
import subprocess
import tempfile
import time


class Rusage(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_ubyte * 16)] + [
        (key, ctypes.c_uint64) for key in (
            "user", "system", "idle_wakeups", "interrupt_wakeups", "pageins",
            "wired", "rss", "footprint", "start", "exit")]


class Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


def usage(pid):
    value = Rusage()
    if lib.proc_pid_rusage(pid, 0, ctypes.byref(value)) != 0:
        raise OSError(ctypes.get_errno(), f"proc_pid_rusage({pid})")
    return {"pid": pid, "memory_bytes": value.footprint, "rss_bytes": value.rss,
            "cpu_ns": (value.user + value.system) * timebase.numer // timebase.denom}


def descendants(roots):
    rows = subprocess.check_output(["/bin/ps", "-axo", "pid=,ppid="], text=True)
    pairs = [tuple(map(int, row.split())) for row in rows.splitlines()]
    result = set(roots)
    while True:
        new = result | {pid for pid, parent in pairs if parent in result}
        if new == result:
            return sorted(result)
        result = new


def wait_until(predicate, seconds=15):
    deadline = time.monotonic() + seconds
    while not predicate():
        if time.monotonic() > deadline:
            raise TimeoutError("benchmark condition did not become true")
        time.sleep(.05)


def status_probe(path, pid, requests):
    # Eight real clients already exist. Fill the remaining job slots with leases,
    # leaving the 64th connection for status. These leases do not run workloads.
    with ExitStack() as leases:
        for _ in range(55):
            conn = leases.enter_context(socket.socket(socket.AF_UNIX))
            conn.settimeout(5)
            conn.connect(path)
            conn.sendall(struct.pack("<4Q", 1, 0, 0, 0))  # SUBMIT
            with conn.makefile("rb") as reader:
                assert struct.unpack("<4Q", reader.read(32))[0] == 10  # QUEUED
        before = usage(pid)
        start = time.monotonic_ns()
        for _ in range(requests):
            with socket.socket(socket.AF_UNIX) as conn:
                conn.settimeout(5)
                conn.connect(path)
                conn.sendall(struct.pack("<4Q", 4, 0, 0, 0))  # STATUS
                with conn.makefile("rb") as reader:
                    frames = list(struct.iter_unpack("<4Q", reader.read()))
            assert len(frames) == 128 and frames[0][0] == 15 and frames[0][3] == 63
            assert frames[-1][0] == 18
            assert sum(f[0] == 16 for f in frames) == 63
            assert sum(f[0] == 17 for f in frames) == 63
            assert sum(f[2] == 2 for f in frames if f[0] == 16) == 1
        elapsed = time.monotonic_ns() - start
        after = usage(pid)
        return {"requests": requests, "jobs": 63, "synthetic_leases": 55,
                "broker_memory_bytes": after["memory_bytes"], "seconds": elapsed / 1e9,
                "broker_cpu_ms": (after["cpu_ns"] - before["cpu_ns"]) / 1e6}


def run_trial(name, paths, seconds, status_queries=0):
    with tempfile.TemporaryDirectory(prefix="rq-cmp-", dir="/tmp") as raw:
        directory = Path(raw)
        env = dict(os.environ)
        # Avoid inheriting queue and allocator overrides from the caller.
        for key in ("RUNQ_JOB_ID", "MallocNanoZone", "MallocSpaceEfficient", "MallocMaxMagazines",
                    "_RUNQ_ALLOCATOR_READY", "_RUNQ_ORIGINAL_MALLOC_MAX_MAGAZINES",
                    "_RUNQ_ORIGINAL_MALLOC_NANO_ZONE"):
            env.pop(key, None)
        starts = directory / "starts"
        script = f"echo $$ >> {shlex.quote(str(starts))}; exec /bin/sleep 300"
        workload = ["/bin/sh", "-c", script]
        processes, clients, commands, detached = [], [], [], []
        daemon = None
        ids = []
        log = (directory / "output.log").open("w+")

        def spawn(command):
            commands.append(command)
            p = subprocess.Popen(command, env=env, cwd=directory,
                                 stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                 start_new_session=True)
            processes.append(p)
            return p

        def call(command):
            commands.append(command)
            return subprocess.check_output(command, env=env, cwd=directory,
                                           stderr=log, text=True, timeout=20).strip()

        def workload_pids():
            return set(map(int, starts.read_text().split())) if starts.exists() else set()

        def stage():
            time.sleep(.3)
            roots = [p.pid for p in processes if p.poll() is None] + detached
            excluded = workload_pids()
            def snapshot():
                values = []
                for pid in descendants(roots):
                    if pid in excluded:
                        continue
                    try:
                        values.append(usage(pid))
                    except ProcessLookupError:
                        pass  # Transient helpers can exit during enumeration.
                return values
            before = {p["pid"]: p for p in snapshot()}
            start = time.monotonic_ns()
            time.sleep(seconds)
            after = snapshot()
            elapsed = time.monotonic_ns() - start
            assert workload_pids() == excluded, "queue unexpectedly admitted another workload"
            return {"processes": after, "manager_memory_bytes": sum(p["memory_bytes"] for p in after),
                    "seconds": elapsed / 1e9,
                    "persistent_cpu_percent_one_core": sum(b["cpu_ns"] - before[b["pid"]]["cpu_ns"] for b in after if b["pid"] in before) / elapsed * 100,
                    "workload_pids_excluded": sorted(excluded)}

        try:
            if name.startswith("runq"):
                base = [paths[name]]
                env["RUNQ_SOCKET"] = str(directory / "s")
                daemon = spawn(base + ["daemon", "--memory", "128MiB", "--cpu-slots", "1"])
                wait_until(lambda: Path(env["RUNQ_SOCKET"]).exists())
                submit = lambda: clients.append(spawn(base + ["exec", "--"] + workload))
                status = lambda: call(base + ["status"])
            elif name == "pueue":
                (directory / "runtime").mkdir()
                config = directory / "pueue.yml"
                config.write_text("shared:\n" + "\n".join(
                    f"  {key}: {json.dumps(str(directory / value))}" for key, value in (
                        ("pueue_directory", "state"), ("runtime_directory", "runtime"),
                        ("unix_socket_path", "s"), ("alias_file", "aliases"))) + "\n")
                base = [paths["pueue"], "-c", str(config)]
                daemon = spawn([paths["pueued"], "-c", str(config)])
                wait_until(lambda: (directory / "s").exists())
                call(base + ["parallel", "1"])
                def submit():
                    ids.append(call(base + ["add", "--print-task-id", shlex.join(workload)]))
                def status():
                    # Pueue includes the caller's environment in full task records.
                    # Retain only queue states in the benchmark artifact.
                    tasks = json.loads(call(base + ["status", "--json"]))["tasks"]
                    return json.dumps({"tasks": {key: {"status": task["status"]}
                                                 for key, task in tasks.items()}})
            elif name == "memmux":
                env["MEMMUX_AGENT_BUDGET_BYTES"] = str(1536 * 2**20)
                repo = directory / "repo"
                repo.mkdir()
                call(["git", "init", "-b", "main", str(repo)])
                call(["git", "-C", str(repo), "-c", "user.name=Benchmark", "-c",
                      "user.email=benchmark@example.invalid", "-c", "commit.gpgsign=false",
                      "commit", "--allow-empty", "-m", "Isolated benchmark"])
                root = directory / "memmux"
                base = [paths["memmux"], "--root", str(root)]
                daemon = spawn(base + ["serve"])
                wait_until(lambda: any(root.glob("*.sock")))
                def submit():
                    response = call(base + ["create", "--title", "sleep", "--repo", str(repo),
                                           "--provider", "generic", "--"] + workload)
                    task = response.split()[0]
                    ids.append(task)
                    call(base + ["start", task])
                status = lambda: call(base + ["list"])
            elif name == "task-spooler":
                for key in list(env):
                    if key.startswith("TS_"):
                        del env[key]
                env["TS_SOCKET"] = str(directory / "s")
                env["TS_SAVELIST"] = str(directory / "saved-jobs")
                base = [paths[name]]
                call(base + ["-S", "1"])
                detached.extend(map(int, call(["/usr/sbin/lsof", "-t", env["TS_SOCKET"]]).split()))
                assert len(detached) == 1, detached
                daemon = detached[0]
                submit = lambda: clients.append(spawn(base + ["-nf"] + workload))
                status = lambda: call(base + ["-l"])
            else:
                env["PARALLEL_HOME"] = str(directory / "parallel")
                Path(env["PARALLEL_HOME"]).mkdir()
                base = [paths["parallel"], "--plain", "--semaphore", "--id", "comparison",
                        "--fg", "--jobs", "1"]
                submit = lambda: clients.append(spawn(base + [shlex.join(workload)]))
                status = lambda: f"{len(clients)} foreground semaphore clients; {len(workload_pids())} workload started"
            result = {"name": name, "idle": stage() if daemon else None}
            submit()
            wait_until(lambda: len(workload_pids()) == 1)
            result["one"] = stage()
            result["one_status"] = status()
            for _ in range(7):
                submit()
            time.sleep(.5)
            assert len(workload_pids()) == 1, "expected one running and seven waiting workloads"
            result["eight"] = stage()
            result["eight_status"] = status()
            if name.startswith("runq"):
                assert result["eight_status"].count("queued") == 7
                if status_queries:
                    result["status_queries"] = status_probe(env["RUNQ_SOCKET"], daemon.pid, status_queries)
            elif name == "pueue":
                states = [t["status"] for t in json.loads(result["eight_status"])["tasks"].values()]
                assert sum("Queued" in s for s in states) == 7, states
                # Also report one waiting client per invocation; no foreground IO equivalence is claimed.
                for task in ids:
                    clients.append(spawn(base + ["wait", "--quiet", task]))
                result["eight_wait_clients"] = stage()
            elif name == "memmux":
                assert result["eight_status"].lower().count("queued") == 7, result["eight_status"]
            elif name == "task-spooler":
                assert result["eight_status"].count("queued") == 7, result["eight_status"]
            result["commands"] = commands.copy()
            return result
        except Exception:
            log.seek(0)
            print(log.read()[-5000:], flush=True)
            raise
        finally:
            # Stop the queue before its workload, so teardown cannot admit queued work.
            roots = [p.pid for p in processes if p.poll() is None] + detached
            for pid in roots:
                try:
                    os.kill(pid, signal.SIGSTOP)
                except ProcessLookupError:
                    pass
            pids = descendants(roots)
            for pid in reversed(pids):
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            for pid in workload_pids():
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            for p in processes:
                p.wait(timeout=10)
            log.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("runq", "runq-before", "pueue", "pueued", "parallel", "memmux", "task-spooler"):
        parser.add_argument("--" + key)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--seconds", type=float, default=2)
    parser.add_argument("--status-queries", type=int, default=0,
                        help="optional runq-only status probe with 63 jobs; 55 use synthetic leases")
    parser.add_argument("--output", default="results/comparison.json")
    args = parser.parse_args()
    if platform.system() != "Darwin":
        parser.error("this comparison uses macOS physical footprint")
    if args.trials < 1 or args.seconds <= 0 or args.status_queries < 0:
        parser.error("trials and seconds must be positive; status-queries must be nonnegative")
    if bool(args.pueue) != bool(args.pueued):
        parser.error("provide both pueue and pueued")
    paths = {k.replace("_", "-"): str(Path(v).resolve()) for k, v in vars(args).items()
             if k in ("runq", "runq_before", "pueue", "pueued", "parallel", "memmux", "task_spooler") and v}
    if not paths:
        parser.error("provide at least one executable")
    lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    lib.proc_pid_rusage.restype = ctypes.c_int
    timebase = Timebase()
    lib.mach_timebase_info.argtypes = [ctypes.POINTER(Timebase)]
    assert lib.mach_timebase_info(ctypes.byref(timebase)) == 0 and timebase.denom
    report = {"platform": platform.platform(), "date": time.strftime("%Y-%m-%d"),
              "metric": "macOS physical footprint; workload memory excluded; CPU percent of one core for processes present at both samples, excludes transient helpers",
              "binaries": {name: {"path": path, "bytes": Path(path).stat().st_size,
                                    "sha256": hashlib.sha256(Path(path).read_bytes()).hexdigest()}
                           for name, path in paths.items()}, "trials": []}
    names = [name for name in paths if name != "pueued"]
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    for trial in range(args.trials):
        # Reverse alternate trials to reduce systematic order bias.
        for name in names if trial % 2 == 0 else reversed(names):
            print(f"trial {trial + 1}: {name}", flush=True)
            result = run_trial(name, paths, args.seconds, args.status_queries)
            result["trial"] = trial + 1
            report["trials"].append(result)
            output.write_text(json.dumps(report, indent=2) + "\n")
            print({key: round(value["manager_memory_bytes"] / 2**20, 3)
                   for key, value in result.items() if isinstance(value, dict) and "manager_memory_bytes" in value}, flush=True)
    report["medians"] = {}
    for name in names:
        rows = [r for r in report["trials"] if r["name"] == name]
        report["medians"][name] = {stage: {
            field: statistics.median(row[stage][field] for row in rows)
            for field in ("manager_memory_bytes", "persistent_cpu_percent_one_core")}
            for stage in ("idle", "one", "eight", "eight_wait_clients") if rows[0].get(stage)}
        if rows[0].get("status_queries"):
            report["medians"][name]["status_queries"] = {
                field: statistics.median(row["status_queries"][field] for row in rows)
                for field in ("broker_memory_bytes", "seconds", "broker_cpu_ms")}
    output.write_text(json.dumps(report, indent=2) + "\n")
