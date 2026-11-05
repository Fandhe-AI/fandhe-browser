"""testnet.py の自己テスト（Issue #1515・TASK-38・`RENDER-3`・`SEC-2`）。宛先検証・ポート帯・帰属の表。"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import listeners  # noqa: E402
import testnet  # noqa: E402


class ValidateEgressDestTest(unittest.TestCase):
    def test_testnet_and_loopback_literals_are_allowed(self) -> None:
        for ip in ("192.0.2.1", "192.0.2.255", "127.0.0.1", "127.0.0.3", "127.255.255.254"):
            testnet.validate_egress_dest(ip, 47300)

    def test_everything_else_is_rejected(self) -> None:
        for ip in ("10.0.0.1", "10.1.2.3", "172.16.0.5", "192.168.1.1", "169.254.169.254", "198.51.100.7",
                   "203.0.113.9", "0.0.0.0", "255.255.255.255", "192.0.3.1", "8.8.8.8", "example.test",
                   "::1", "2001:db8::1", "", "192.0.2.1 ", "192.0.2"):
            with self.assertRaises(ValueError, msg=ip):
                testnet.validate_egress_dest(ip, 47300)

    def test_non_string_ip_is_rejected(self) -> None:
        for ip in (None, 3232235521, b"192.0.2.1"):
            with self.assertRaises(ValueError):
                testnet.validate_egress_dest(ip, 47300)  # type: ignore[arg-type]

    def test_port_must_be_an_int_of_1024_or_more(self) -> None:
        for port in (0, 80, 1023, 65536, -1, True, "47300", 1.5):
            with self.assertRaises(ValueError, msg=repr(port)):
                testnet.validate_egress_dest("192.0.2.1", port)  # type: ignore[arg-type]
        testnet.validate_egress_dest("192.0.2.1", 1024)
        testnet.validate_egress_dest("192.0.2.1", 65535)

    def test_listener_bind_rule_is_unchanged(self) -> None:
        listeners.validate_bind("127.0.0.2", 0)
        with self.assertRaises(ValueError):
            listeners.validate_bind("192.0.2.1", 47300)


class PortsTest(unittest.TestCase):
    def test_route_names_and_order(self) -> None:
        self.assertEqual(testnet.TESTNET_ROUTES,
                         ("testnet_redirect", "testnet_img", "testnet_fetch", "testnet_ws", "testnet_udp"))

    def test_port_bands_are_fixed_and_do_not_overlap(self) -> None:
        self.assertEqual(testnet.testnet_ports("baseline"),
                         {"testnet_redirect": 47300, "testnet_img": 47301, "testnet_fetch": 47302,
                          "testnet_ws": 47303, "testnet_udp": 47304})
        self.assertEqual(testnet.testnet_ports("appcontainer")["testnet_redirect"], 47316)
        allports = [p for m in testnet.TESTNET_MODES for p in testnet.testnet_ports(m).values()]
        self.assertEqual(len(allports), len(set(allports)))
        with self.assertRaises(ValueError):
            testnet.testnet_ports("seatbelt")

    def test_port_index_attributes_by_proto_and_port(self) -> None:
        idx = testnet.port_index()
        self.assertEqual(idx[("tcp", 47303)], ("baseline", "testnet_ws"))
        self.assertEqual(idx[("udp", 47304)], ("baseline", "testnet_udp"))
        self.assertEqual(idx[("tcp", 47319)], ("appcontainer", "testnet_ws"))
        self.assertEqual(idx[("udp", 47320)], ("appcontainer", "testnet_udp"))
        self.assertNotIn(("tcp", 47304), idx)  # UDP の経路のポートは TCP では引けない
        self.assertEqual(len(idx), 10)

    def test_in_testnet_and_route_key(self) -> None:
        self.assertTrue(testnet.in_testnet("192.0.2.9"))
        self.assertFalse(testnet.in_testnet("127.0.0.1"))
        self.assertFalse(testnet.in_testnet("nope"))
        self.assertEqual(testnet.route_key("baseline", "testnet_img"), "baseline/testnet_img")


if __name__ == "__main__":
    unittest.main()
