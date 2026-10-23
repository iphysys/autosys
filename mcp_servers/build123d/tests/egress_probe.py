"""Probes whether this container can reach the internet.

Standard library only, so it also runs inside the server's own image without
adding anything to it; `cargo xtask test-tools` pipes it in on stdin:

    docker compose exec -T build123d python -I - 127.0.0.1:8000 < tests/egress_probe.py

The argument is a control target that must be reachable, so a probe that
cannot connect to anything does not pass. Prints one JSON report and exits 0
only when every external probe fails and the control succeeds.
"""

import json
import socket
import struct
import sys

TIMEOUT_SECONDS = 3.0

EXTERNAL_NAMES = ("example.com", "pypi.org")
# By address, so a DNS failure alone cannot hide an open route.
EXTERNAL_TCP = (("1.1.1.1", 443), ("8.8.8.8", 53), ("9.9.9.9", 443))
EXTERNAL_DNS_SERVERS = (("8.8.8.8", 53), ("1.1.1.1", 53))


def resolves(name: str) -> bool:
    try:
        socket.getaddrinfo(name, 443, proto=socket.IPPROTO_TCP)
    except OSError:
        return False
    return True


def tcp_connects(host: str, port: int) -> bool:
    try:
        with socket.create_connection((host, port), timeout=TIMEOUT_SECONDS):
            return True
    except OSError:
        return False


def dns_server_answers(host: str, port: int) -> bool:
    """Sends an A query for example.com straight to a public resolver over UDP."""
    header = struct.pack(">HHHHHH", 0x5A17, 0x0100, 1, 0, 0, 0)
    question = b"\x07example\x03com\x00" + struct.pack(">HH", 1, 1)
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.settimeout(TIMEOUT_SECONDS)
            sock.sendto(header + question, (host, port))
            data, _ = sock.recvfrom(512)
    except OSError:
        return False
    return len(data) > 0


def probe(control_host: str, control_port: int) -> dict[str, dict[str, bool]]:
    return {
        "dns": {name: resolves(name) for name in EXTERNAL_NAMES},
        "tcp": {f"{h}:{p}": tcp_connects(h, p) for h, p in EXTERNAL_TCP},
        "udp_dns": {f"{h}:{p}": dns_server_answers(h, p) for h, p in EXTERNAL_DNS_SERVERS},
        "control": {f"{control_host}:{control_port}": tcp_connects(control_host, control_port)},
    }


def isolated(report: dict[str, dict[str, bool]]) -> bool:
    external = [
        ok for kind, results in report.items() if kind != "control" for ok in results.values()
    ]
    return not any(external) and all(report["control"].values())


def main(argv: list[str]) -> int:
    if len(argv) != 1 or ":" not in argv[0]:
        print("usage: egress_probe.py <control-host>:<control-port>", file=sys.stderr)
        return 2
    host, port = argv[0].rsplit(":", 1)
    report = probe(host, int(port))
    print(json.dumps({"isolated": isolated(report), "report": report}, sort_keys=True))
    return 0 if isolated(report) else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
