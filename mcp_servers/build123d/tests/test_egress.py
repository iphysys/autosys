"""The network the server runs on has no route to the internet.

Runs in the container suite (`docker compose run build123d-test`), which sets
BUILD123D_ISOLATION_TEST=1 and shares the server's internal network. On a
developer machine the network is not the one under test, so it is skipped.
"""

import os
from urllib.parse import urlsplit

import pytest

import egress_probe

requires_isolated_network = pytest.mark.skipif(
    os.environ.get("BUILD123D_ISOLATION_TEST") != "1",
    reason="runs only on the compose network (BUILD123D_ISOLATION_TEST=1)",
)


@requires_isolated_network
def test_no_external_name_resolves() -> None:
    assert {n: egress_probe.resolves(n) for n in egress_probe.EXTERNAL_NAMES} == dict.fromkeys(
        egress_probe.EXTERNAL_NAMES, False
    )


@requires_isolated_network
def test_no_public_address_accepts_a_connection() -> None:
    reached = [f"{h}:{p}" for h, p in egress_probe.EXTERNAL_TCP if egress_probe.tcp_connects(h, p)]
    assert reached == []


@requires_isolated_network
def test_no_public_resolver_answers() -> None:
    answered = [
        f"{h}:{p}"
        for h, p in egress_probe.EXTERNAL_DNS_SERVERS
        if egress_probe.dns_server_answers(h, p)
    ]
    assert answered == []


@requires_isolated_network
def test_the_server_is_reachable_on_the_same_network(server_url: str) -> None:
    # The control: the probes above fail because of the network, not because
    # this container cannot connect to anything.
    parts = urlsplit(server_url)
    assert parts.hostname is not None and parts.port is not None
    assert egress_probe.tcp_connects(parts.hostname, parts.port)


def test_the_probe_reports_isolation_only_with_a_reachable_control() -> None:
    blocked = {"dns": {"a": False}, "tcp": {"b": False}, "udp_dns": {"c": False}}
    assert egress_probe.isolated(blocked | {"control": {"x": True}})
    assert not egress_probe.isolated(blocked | {"control": {"x": False}})
    assert not egress_probe.isolated(blocked | {"tcp": {"b": True}} | {"control": {"x": True}})
