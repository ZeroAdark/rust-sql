#!/usr/bin/env python3
"""Exercise server TLS and authentication on an owned loopback-only cluster.

Requires Linux, OpenSSL, psycopg 3 and the candidate postgres/initdb binaries.
Ephemeral keys/passwords are confined to a private temporary cluster, not reports.
"""
import argparse
import hashlib
import json
from pathlib import Path
import secrets
import socket
import ssl
import struct
import subprocess
import time

import psycopg
from psycopg import sql
from cluster import Cluster


def certificates(root):
    def openssl(*args):
        result = subprocess.run(["openssl", *map(str, args)], cwd=root, capture_output=True,
                                text=True, timeout=30)
        if result.returncode:
            raise RuntimeError("certificate generation failed: " + result.stderr)

    for ca in ("ca", "wrong-ca"):
        openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
                "-subj", f"/CN=Rust SQL disposable {ca}", "-keyout", f"{ca}.key",
                "-out", f"{ca}.crt", "-addext", "basicConstraints=critical,CA:TRUE",
                "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    for name, cn, usage, serial in (("server", "localhost", "serverAuth", 1),
                                    ("client", "cert_user", "clientAuth", 2),
                                    ("wrong-client", "other_user", "clientAuth", 3)):
        openssl("req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={cn}",
                "-keyout", f"{name}.key", "-out", f"{name}.csr")
        extensions = "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\n"
        extensions += f"extendedKeyUsage={usage}\n"
        if name == "server":
            extensions += "subjectAltName=DNS:localhost,IP:127.0.0.1\n"
        (root / f"{name}.ext").write_text(extensions)
        openssl("x509", "-req", "-in", f"{name}.csr", "-CA", "ca.crt", "-CAkey", "ca.key",
                "-set_serial", serial, "-days", "2", "-sha256", "-extfile", f"{name}.ext",
                "-out", f"{name}.crt")
    for key in root.glob("*.key"):
        key.chmod(0o600)


class SecureCluster(Cluster):
    def start(self):
        if not getattr(self, "configured", False):
            certificates(self.root)
            # Install rejection rules BEFORE opening a TCP listener. Only the
            # administrator's private Unix socket uses trust authentication.
            (self.data / "pg_hba.conf").write_text(
                "local all all trust\n"
                "hostnossl all all 127.0.0.1/32 reject\n"
                "hostssl all cert_user 127.0.0.1/32 cert\n"
                "hostssl all all 127.0.0.1/32 scram-sha-256\n")
            self.settings += ["listen_addresses=127.0.0.1", "ssl=on",
                              f"ssl_cert_file={self.root / 'server.crt'}",
                              f"ssl_key_file={self.root / 'server.key'}",
                              f"ssl_ca_file={self.root / 'ca.crt'}",
                              "ssl_min_protocol_version=TLSv1.2", "password_encryption=scram-sha-256"]
            self.configured = True
        super().start()


def peer_fingerprint(ca):
    context = ssl.create_default_context(cafile=ca)
    with socket.create_connection(("127.0.0.1", 55440), timeout=3) as wire:
        wire.sendall(struct.pack("!II", 8, 80877103))  # PostgreSQL SSLRequest
        if wire.recv(1) != b"S":
            raise AssertionError("server refused TLS negotiation")
        with context.wrap_socket(wire, server_hostname="localhost") as tls:
            return hashlib.sha256(tls.getpeercert(binary_form=True)).hexdigest()


def wait_for(predicate, description):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise TimeoutError(description)


def run(args):
    report = {"passed": False, "checks": [], "failures": []}
    with (args.candidate / "postgres").open("rb") as binary:
        report["binary_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
    # Never stop or connect to an unrelated service if the test port is occupied.
    with socket.socket() as preflight:
        preflight.bind(("127.0.0.1", 55440))
    cluster = SecureCluster(args.candidate, args.work_dir)
    report["cluster"] = str(cluster.root)
    password = secrets.token_urlsafe(32)
    try:
        with cluster, psycopg.connect(cluster.dsn, autocommit=True) as admin:
            report["version"] = cluster.version
            report["client"] = {"psycopg": psycopg.__version__, "libpq": psycopg.pq.version()}
            for user, options in (("scram_user", "LOGIN"), ("no_login", "NOLOGIN"),
                                  ("expired_user", "LOGIN VALID UNTIL '2000-01-01'")):
                admin.execute(sql.SQL("CREATE ROLE {} {} PASSWORD {}").format(
                    sql.Identifier(user), sql.SQL(options), sql.Literal(password)))
            admin.execute("CREATE ROLE cert_user LOGIN")
            admin.execute("CREATE DATABASE forbidden")
            admin.execute("REVOKE CONNECT ON DATABASE forbidden FROM PUBLIC")
            stored = admin.execute("SELECT rolpassword LIKE 'SCRAM-SHA-256$%' FROM pg_authid WHERE rolname='scram_user'").fetchone()[0]
            if not stored:
                raise AssertionError("password was not stored as SCRAM-SHA-256")
            report["checks"].append({"name": "scram_storage", "passed": True})
            base = dict(host="localhost", hostaddr="127.0.0.1", port=55440, user="scram_user",
                        dbname="postgres", password=password, connect_timeout=3, sslmode="verify-full",
                        sslrootcert=str(cluster.root / "ca.crt"), sslcert=str(cluster.root / "absent.crt"),
                        sslkey=str(cluster.root / "absent.key"), passfile=str(cluster.root / "absent-passfile"),
                        channel_binding="require", autocommit=True)

            def check(name, overrides=None, rejects=(), protocol=None):
                params = {**base, **(overrides or {})}
                try:
                    with psycopg.connect(**params) as conn:
                        row = conn.execute("SELECT current_user,ssl,version,bits FROM pg_stat_ssl WHERE pid=pg_backend_pid()").fetchone()
                        if rejects:
                            raise AssertionError("connection unexpectedly accepted")
                        if not row or row[0] != params["user"] or row[1] is not True or row[3] < 128:
                            raise AssertionError(f"unexpected session identity/TLS properties: {row}")
                        if protocol and row[2] != protocol:
                            raise AssertionError(f"wrong TLS version: {row[2]}")
                        conn.execute("SELECT 42").fetchone()
                        report["checks"].append({"name": name, "passed": True, "tls": row[2], "bits": row[3]})
                except psycopg.OperationalError as exc:
                    message = str(exc).replace(password, "<ephemeral password>")
                    if rejects and any(phrase in message.lower() for phrase in rejects):
                        report["checks"].append({"name": name, "passed": True, "rejected": message})
                    else:
                        report["failures"].append({"name": name, "error": message})
                except Exception as exc:
                    report["failures"].append({"name": name, "error": str(exc).replace(password, "<ephemeral password>")})
                # A rejection caused by a server crash is never a passing check.
                if admin.execute("SELECT 42").fetchone() != (42,):
                    raise AssertionError("server unhealthy after authentication check")

            check("verify_full_scram_channel_binding")
            check("tls_12", {"ssl_min_protocol_version": "TLSv1.2", "ssl_max_protocol_version": "TLSv1.2"}, protocol="TLSv1.2")
            check("tls_13", {"ssl_min_protocol_version": "TLSv1.3", "ssl_max_protocol_version": "TLSv1.3"}, protocol="TLSv1.3")
            check("incorrect_password", {"password": "incorrect-" + password}, ("password authentication failed",))
            check("unknown_role", {"user": "unknown_role"}, ("password authentication failed",))
            check("no_login_role", {"user": "no_login"}, ("is not permitted to log in",))
            check("expired_password", {"user": "expired_user"}, ("password authentication failed",))
            check("database_connect_denied", {"dbname": "forbidden"}, ("permission denied for database",))
            check("no_tcp_trust_for_admin", {"user": "rustsql", "password": ""}, ("no password supplied", "password authentication failed"))
            check("plaintext_rejected", {"sslmode": "disable", "channel_binding": "disable"}, ("pg_hba.conf rejects connection",))
            check("wrong_ca_rejected", {"sslrootcert": str(cluster.root / "wrong-ca.crt")}, ("certificate verify failed",))
            check("wrong_hostname_rejected", {"host": "wrong.invalid"}, ("does not match host name",))
            # Channel binding applies to SCRAM, not certificate-only authentication.
            cert = {"user": "cert_user", "password": "", "channel_binding": "disable"}
            check("missing_client_certificate", cert, ("requires a valid client certificate",))
            check("client_certificate", {**cert, "sslcert": str(cluster.root / "client.crt"), "sslkey": str(cluster.root / "client.key")})
            check("wrong_certificate_identity", {**cert, "sslcert": str(cluster.root / "wrong-client.crt"),
                                                   "sslkey": str(cluster.root / "wrong-client.key")}, ("certificate authentication failed",))
            with psycopg.connect(**base) as persistent:
                old_certificate = (cluster.root / "server.crt").read_bytes()
                old_fingerprint = peer_fingerprint(cluster.root / "ca.crt")
                with (cluster.root / "server.log").open("rb") as log:
                    log.seek(0, 2)
                    pending = bytearray()

                    def reload_failed():
                        pending.extend(log.read())
                        return b"could not load server certificate file" in pending

                    (cluster.root / "server.crt").write_text("deliberately malformed test certificate\n")
                    admin.execute("SELECT pg_reload_conf()")
                    wait_for(reload_failed, "server did not report invalid certificate reload")
                if peer_fingerprint(cluster.root / "ca.crt") != old_fingerprint:
                    raise AssertionError("failed certificate reload replaced the live TLS context")
                check("failed_reload_retains_valid_context")
                (cluster.root / "server.crt").write_bytes(old_certificate)
                subprocess.run(["openssl", "x509", "-req", "-in", "server.csr", "-CA", "ca.crt",
                                "-CAkey", "ca.key", "-set_serial", "99", "-days", "2", "-sha256",
                                "-extfile", "server.ext", "-out", "server-next.crt"],
                               cwd=cluster.root, capture_output=True, check=True, timeout=30)
                next_der = ssl.PEM_cert_to_DER_cert((cluster.root / "server-next.crt").read_text())
                next_fingerprint = hashlib.sha256(next_der).hexdigest()
                (cluster.root / "server-next.crt").replace(cluster.root / "server.crt")
                admin.execute("SELECT pg_reload_conf()")
                wait_for(lambda: peer_fingerprint(cluster.root / "ca.crt") == next_fingerprint,
                         "new connections did not use the rotated certificate")
                check("scram_channel_binding_after_certificate_rotation")
                if persistent.execute("SELECT 42").fetchone() != (42,):
                    raise AssertionError("existing session failed across certificate reloads")
                report["checks"].append({"name": "existing_session_survives_certificate_rotation", "passed": True})
                report["certificate_rotation"] = {"before_sha256": old_fingerprint, "after_sha256": next_fingerprint}
        key = cluster.root / "server.key"
        with (cluster.root / "server.log").open("rb") as log:
            log.seek(0, 2)
            key.chmod(0o644)
            try:
                try:
                    cluster.start()
                except RuntimeError:
                    if b"has group or world access" not in log.read():
                        raise
                    report["checks"].append({"name": "insecure_private_key_permissions_reject_startup", "passed": True})
                else:
                    raise AssertionError("server accepted a private key with mode 0644")
            finally:
                cluster.stop()
                key.chmod(0o600)
        try:
            cluster.start()
            with psycopg.connect(**base) as conn:
                if conn.execute("SELECT 42").fetchone() != (42,):
                    raise AssertionError("TLS/SCRAM connection failed after repairing private key permissions")
            report["checks"].append({"name": "secure_restart_after_permission_repair", "passed": True})
        finally:
            cluster.stop()
    except Exception as exc:
        report["failures"].append({"name": "probe", "error": str(exc).replace(password, "<ephemeral password>")})
    report["passed"] = bool(report["checks"]) and not report["failures"]
    return report


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--candidate", type=Path, required=True)
    ap.add_argument("--work-dir", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    report = run(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
