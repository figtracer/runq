#!/usr/bin/env python3
"""Exercise the release binary with real subprocesses, without large allocations."""
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest

BIN = Path(os.environ.get("RUNQ_BIN", "target/release/runq")).resolve()
PYTHON = sys.executable


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def wait_for(predicate, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(0.025)
    raise AssertionError("condition did not become true")


class Integration(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rq-test-", dir="/tmp")
        self.root = Path(self.temp.name)
        self.env = {**os.environ, "RUNQ_SOCKET": str(self.root / "s")}
        self.env.pop("RUNQ_JOB_ID", None)
        self.children = []
        self.log = open(self.root / "daemon.log", "w+")
        self.daemon = subprocess.Popen(
            [str(BIN), "daemon", "--memory", "128MiB", "--sample-ms", "50"],
            env=self.env, stdout=self.log, stderr=self.log,
        )
        wait_for(lambda: (self.root / "s").exists() or self.daemon.poll() is not None)
        if self.daemon.poll() is not None:
            self.log.seek(0)
            self.fail(self.log.read())

    def tearDown(self):
        for child in self.children:
            if child.poll() is None:
                child.terminate()
        for child in self.children:
            try:
                child.communicate(timeout=6)
            except subprocess.TimeoutExpired:
                child.kill()
                child.communicate(timeout=3)
        if self.daemon.poll() is None:
            self.daemon.terminate()
        try:
            self.daemon.wait(timeout=6)
        except subprocess.TimeoutExpired:
            self.daemon.kill()
            self.daemon.wait()
        self.log.close()
        self.temp.cleanup()

    def cli(self, *args):
        return subprocess.run([str(BIN), *args], env=self.env, capture_output=True, timeout=8, text=True)

    def start(self, code, *options):
        p = subprocess.Popen([str(BIN), "exec", *options, "--", PYTHON, "-c", code],
                             env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.children.append(p)
        return p

    def test_exit_arguments_environment_and_stdio(self):
        script = "import os,sys; print(repr(sys.argv[1:])); print(os.environ['CARGO_BUILD_JOBS']); print(sys.stdin.read()); print('err',file=sys.stderr); sys.exit(7)"
        r = subprocess.run([str(BIN), "exec", "--", PYTHON, "-c", script, "a b", "$(literal)", ""],
                           env=self.env, input="piped input", capture_output=True, text=True, timeout=8)
        self.assertEqual(r.returncode, 7, r.stderr)
        self.assertIn("['a b', '$(literal)', '']\n2\npiped input", r.stdout)
        self.assertIn("err\n", r.stderr)

    def test_internal_descriptors_do_not_reach_workload(self):
        script = """import os
open_fds = []
for fd in range(3, 256):
    try:
        os.fstat(fd)
        open_fds.append(fd)
    except OSError:
        pass
print(open_fds)
"""
        p = self.start(script)
        out, err = p.communicate(timeout=8)
        self.assertEqual(p.returncode, 0, err)
        self.assertEqual(out.strip(), "[]", err)

    @unittest.skipUnless(sys.platform == "darwin", "macOS allocator policy")
    def test_allocator_policy_does_not_change_workload_environment(self):
        script = "import os; print(repr(os.environ.get('MallocMaxMagazines'))); print(sorted(k for k in os.environ if k in ('_RUNQ_ALLOCATOR_READY', '_RUNQ_ORIGINAL_MALLOC_MAX_MAGAZINES')))"
        for value in (None, "1", "2"):
            with self.subTest(original=value):
                env = dict(self.env)
                env.pop("_RUNQ_ALLOCATOR_READY", None)
                env.pop("_RUNQ_ORIGINAL_MALLOC_MAX_MAGAZINES", None)
                if value is None:
                    env.pop("MallocMaxMagazines", None)
                else:
                    env["MallocMaxMagazines"] = value
                r = subprocess.run([str(BIN), "exec", "--", PYTHON, "-c", script],
                                   env=env, capture_output=True, text=True, timeout=8)
                self.assertEqual(r.returncode, 0, r.stderr)
                self.assertEqual(r.stdout.splitlines(), [repr(value), "[]"], r.stderr)

    def test_commands_serialize_across_clients(self):
        marker = self.root / "first"
        p1 = self.start(f"import pathlib,time; pathlib.Path({str(marker)!r}).write_text('running'); time.sleep(.6); print('first')")
        wait_for(marker.exists)
        p2 = self.start(f"import pathlib; assert pathlib.Path({str(marker)!r}).exists(); print('second')")
        wait_for(lambda: "state=queued" in self.cli("status").stdout)
        self.assertIsNone(p2.poll())
        self.assertEqual(p1.communicate(timeout=8)[0].strip(), "first")
        self.assertEqual(p2.communicate(timeout=8)[0].strip(), "second")
        self.assertEqual((p1.returncode, p2.returncode), (0, 0))
        self.assertIn("jobs=0", self.cli("status").stdout)

    def test_memory_breach_kills_and_releases_capacity(self):
        start = time.monotonic()
        p = self.start("import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); data=bytearray(64*1024*1024); time.sleep(15)", "--memory", "32MiB")
        _, err = p.communicate(timeout=8)
        self.assertEqual(p.returncode, 125, err)
        self.assertIn("memory budget exceeded", err)
        # A memory breach must not wait through the two-second cancellation grace.
        self.assertLess(time.monotonic() - start, 2)
        self.assertEqual(self.cli("exec", "--", "/bin/echo", "next").returncode, 0)

    def test_group_memory_includes_children(self):
        code = "import subprocess,sys,time; p=subprocess.Popen([sys.executable,'-c','import time; a=bytearray(48*1024*1024); time.sleep(15)']); p.wait()"
        p = self.start(code, "--memory", "40MiB")
        _, err = p.communicate(timeout=8)
        self.assertEqual(p.returncode, 125, err)
        self.assertIn("memory budget exceeded", err)

    def test_timeout(self):
        p = self.start("import time; time.sleep(15)", "--timeout", "1")
        _, err = p.communicate(timeout=8)
        self.assertEqual(p.returncode, 125, err)
        self.assertIn("deadline exceeded", err)

    def test_success_cleans_background_children(self):
        marker = self.root / "child"
        p = self.start(f"import pathlib,subprocess; c=subprocess.Popen(['/bin/sleep','20']); pathlib.Path({str(marker)!r}).write_text(str(c.pid))")
        _, err = p.communicate(timeout=8)
        self.assertEqual(p.returncode, 0, err)
        wait_for(lambda: not alive(int(marker.read_text())))

    def test_cpu_accounting_uses_nanoseconds(self):
        marker = self.root / "cpu"
        p = self.start(f"import os,pathlib,time; start=time.process_time();\nwhile time.process_time()-start < .2: pass\npathlib.Path({str(marker)!r}).write_text(str(os.getpid())); time.sleep(2)")
        wait_for(marker.exists)
        report = self.cli("inspect", marker.read_text())
        self.assertEqual(report.returncode, 0, report.stderr)
        cpu_ns = int(dict(part.split('=') for part in report.stdout.split())["cpu_ns"])
        self.assertGreaterEqual(cpu_ns, 180_000_000)
        p.communicate(timeout=8)

    def test_idle_daemon_shutdown_signal(self):
        self.daemon.terminate()
        self.assertEqual(self.daemon.wait(timeout=3), 0)
        self.assertFalse((self.root / "s").exists())

    def test_stale_socket_recovery(self):
        self.daemon.kill()
        self.daemon.wait()
        self.daemon = subprocess.Popen([str(BIN), "daemon", "--memory", "128MiB"],
                                       env=self.env, stdout=self.log, stderr=self.log)
        wait_for(lambda: self.cli("status").returncode == 0)
        self.assertEqual(self.cli("exec", "--", "/usr/bin/true").returncode, 0)

    def test_reservations_limit_parallel_jobs(self):
        self.daemon.terminate()
        self.daemon.wait(timeout=3)
        self.daemon = subprocess.Popen([str(BIN), "daemon", "--memory", "64MiB",
                                       "--cpu-slots", "4", "--max-running", "3"],
                                       env=self.env, stdout=self.log, stderr=self.log)
        wait_for(lambda: (self.root / "s").exists())
        p1 = self.start("import time; time.sleep(.8)", "--memory", "40MiB", "--cpu-slots", "2")
        wait_for(lambda: "state=running" in self.cli("status").stdout)
        p2 = self.start("pass", "--memory", "40MiB", "--cpu-slots", "2")
        wait_for(lambda: "state=queued" in self.cli("status").stdout)
        self.assertIsNone(p2.poll())
        p1.communicate(timeout=8)
        p2.communicate(timeout=8)
        self.assertEqual((p1.returncode,p2.returncode), (0,0))

    def test_cancel_escalates_and_cleans_grandchild(self):
        pidfile = self.root / "grandchild"
        child = f"import os,pathlib,signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); pathlib.Path({str(pidfile)!r}).write_text(str(os.getpid())); time.sleep(20)"
        p = self.start(f"import subprocess,sys,signal; signal.signal(signal.SIGTERM,signal.SIG_IGN); subprocess.Popen([sys.executable,'-c',{child!r}]).wait()")
        wait_for(pidfile.exists)
        pid = int(pidfile.read_text())
        self.assertEqual(self.cli("cancel", "1").returncode, 0)
        _, err = p.communicate(timeout=8)
        self.assertEqual(p.returncode, 125, err)
        wait_for(lambda: not alive(pid))
        self.assertIn("jobs=0", self.cli("status").stdout)

    def test_cancel_queued_job_never_starts(self):
        p1 = self.start("import time; time.sleep(1)")
        wait_for(lambda: "state=running" in self.cli("status").stdout)
        marker = self.root / "must-not-exist"
        p2 = self.start(f"import pathlib; pathlib.Path({str(marker)!r}).touch()")
        wait_for(lambda: "state=queued" in self.cli("status").stdout)
        self.assertEqual(self.cli("cancel", "2").returncode, 0)
        p2.communicate(timeout=8)
        p1.communicate(timeout=8)
        self.assertNotEqual(p2.returncode, 0)
        self.assertFalse(marker.exists())

    def test_broker_death_stops_workload(self):
        marker = self.root / "pid"
        p = self.start(f"import os,pathlib,time; pathlib.Path({str(marker)!r}).write_text(str(os.getpid())); time.sleep(20)")
        wait_for(marker.exists)
        pid = int(marker.read_text())
        self.daemon.kill()
        self.daemon.wait()
        _, err = p.communicate(timeout=8)
        self.assertEqual(p.returncode, 125, err)
        wait_for(lambda: not alive(pid))

    def test_supervisor_death_stops_workload(self):
        marker = self.root / "pid"
        p = self.start(f"import os,pathlib,time; pathlib.Path({str(marker)!r}).write_text(str(os.getpid())); time.sleep(20)")
        wait_for(marker.exists)
        pid = int(marker.read_text())
        p.kill()
        p.communicate(timeout=8)
        wait_for(lambda: not alive(pid))
        wait_for(lambda: "jobs=0" in self.cli("status").stdout)

    def test_signal_is_forwarded(self):
        marker = self.root / "ready"
        p = self.start(f"import pathlib,time; pathlib.Path({str(marker)!r}).touch(); time.sleep(20)")
        wait_for(marker.exists)
        p.send_signal(signal.SIGINT)
        p.communicate(timeout=8)
        self.assertEqual(p.returncode, 130)

    def test_capacity_worker_override_and_nested_rejected(self):
        self.assertEqual(self.cli("exec", "--memory", "1GiB", "--", "/usr/bin/true").returncode, 125)
        self.assertEqual(self.cli("exec", "--", "cargo", "test", "-j8").returncode, 125)
        nested = self.cli("exec", "--", str(BIN), "exec", "--", "/usr/bin/true")
        self.assertEqual(nested.returncode, 125, nested.stderr)
        self.assertIn("nested", nested.stderr)

    def test_duplicate_daemon_does_not_unlink_active_socket(self):
        second = self.cli("daemon")
        self.assertEqual(second.returncode, 125)
        self.assertEqual(self.cli("status").returncode, 0)

    def test_fragmented_wire_and_malformed_client(self):
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(self.env["RUNQ_SOCKET"])
            frame = struct.pack("<4Q", 4, 0, 0, 0)
            for b in frame:
                s.sendall(bytes([b]))
            s.settimeout(3)
            data = b""
            while len(data) < 32:
                data += s.recv(32-len(data))
            self.assertEqual(struct.unpack("<4Q", data)[0], 15)
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(self.env["RUNQ_SOCKET"])
            s.sendall(struct.pack("<4Q", 999, 0, 0, 0))
            s.settimeout(3)
            self.assertEqual(struct.unpack("<4Q", s.recv(32))[0], 14)
        self.assertEqual(self.cli("status").returncode, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
