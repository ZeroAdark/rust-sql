"""Owned Linux clusters for differential and failure-injection validation."""
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


class Cluster:
    def __init__(self, binaries, work_dir, *, reference=False, settings=()):
        if os.geteuid() == 0:
            raise RuntimeError("run validation as a non-root user")
        self.binaries = Path(binaries).resolve()
        Path(work_dir).mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="compat-", dir=work_dir))
        self.data, self.socket = self.root / "data", self.root / "socket"
        self.socket.mkdir(mode=0o700)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("PG")}
        self.env.update(PGHOST=str(self.socket), PGPORT="55440", PGUSER="rustsql", PGDATABASE="postgres",
                        PGCONNECT_TIMEOUT="3", RUST_MIN_STACK="33554432", PGAPPNAME="rustsql-validation")
        self.reference, self.settings = reference, list(settings)
        self.process, self.log = None, None

    @property
    def dsn(self):
        from psycopg.conninfo import make_conninfo
        return make_conninfo(host=str(self.socket), port=55440, user="rustsql", dbname="postgres", connect_timeout=3)

    def __enter__(self):
        init = subprocess.run([str(self.binaries / "initdb"), "-D", str(self.data), "--no-locale",
                               "--encoding=UTF8", "-U", "rustsql", "--auth=trust"],
                              env=self.env, capture_output=True, text=True, timeout=120)
        (self.root / "initdb.log").write_text(init.stdout + init.stderr)
        init.check_returncode()
        try:
            self.start()
        except BaseException:
            self.stop()
            raise
        return self

    def start(self):
        import psycopg
        if self.process is not None and self.process.poll() is None:
            raise RuntimeError("cluster already running")
        cmd = [str(self.binaries / "postgres"), "-D", str(self.data), "-k", str(self.socket), "-p", "55440",
               "-c", "listen_addresses=", "-c", "io_method=sync"]
        if not self.reference:
            cmd += ["-c", "max_stack_depth=60000"]
        for setting in self.settings:
            cmd += ["-c", setting]
        self.log = (self.root / "server.log").open("a")
        # A private process group allows SIGKILL to reach the supervisor AND server child.
        self.process = subprocess.Popen(cmd, env=self.env, stdout=self.log, stderr=self.log, start_new_session=True)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"server exited; see {self.root / 'server.log'}")
            try:
                with psycopg.connect(self.dsn, autocommit=True) as conn:
                    version = conn.execute("SELECT version()").fetchone()[0]
                    if ("pgrust" in version) == self.reference:
                        raise RuntimeError(f"unexpected server: {version}")
                    self.version = version
                    return
            except psycopg.OperationalError:
                time.sleep(0.1)
        raise TimeoutError(f"server startup; see {self.root}")

    def stop(self, crash=False):
        if self.process is not None and self.process.poll() is None:
            if crash:
                os.killpg(self.process.pid, signal.SIGKILL)
            else:
                self.process.send_signal(signal.SIGINT)
            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=10)
                raise
        if self.log is not None:
            self.log.close()
        self.process, self.log = None, None

    def __exit__(self, *_):
        self.stop()
