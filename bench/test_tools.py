"""Unit tests for bench/from-museum's labelling and bench/check-trace's
leak patterns. Run by tests/bench_tools.rs, so `cargo test` covers them;
directly: python3 -m unittest discover -s bench -p 'test_*.py'."""

import importlib.machinery
import importlib.util
import unittest
from pathlib import Path

BENCH = Path(__file__).resolve().parent


def load(name):
    loader = importlib.machinery.SourceFileLoader(name.replace("-", "_"), str(BENCH / name))
    mod = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    loader.exec_module(mod)
    return mod


fm = load("from-museum")
ct = load("check-trace")


class Light(unittest.TestCase):
    """Spec §Trace format: waits and polls are light however long they take."""

    def test_waits_are_light_even_when_long(self):
        for cmd in [
            "mu wait refuter-1 --timeout 1800",
            "mu agent wait worker-17 -w x --after-runs 0 --timeout 1500 --json",
            "mu task wait a/b --first --on-stall exit --json",
            "sleep 900",
            "tail -f /tmp/gate.log",
            "tail --follow=name /tmp/gate.log",
            "museum-search calls --min-secs 30",
            "~/skills/museum/scripts/museum-search find gate",
            "timeout 175 mu agent wait r -w scratch --timeout 170 --json | head -c 7000",
            "cd ~; sleep 60; tail -5 x.log; ps -p 12 -o etime=",
        ]:
            with self.subTest(cmd=cmd):
                self.assertEqual(fm.label(cmd, 1800, None), "light")

    def test_a_poll_of_a_gate_log_is_light_though_the_rule_matches(self):
        self.assertEqual(fm.label("sleep 420; tail -4 /tmp/w21/gate.log", 420, "gate"), "light")
        self.assertEqual(fm.label("tail -5 contrib/gate.log", 31, "gate"), "light")

    def test_a_matched_rule_wins_over_a_wait_elsewhere(self):
        self.assertEqual(fm.label("sleep 5; contrib/gate", 300, "gate"), "gate")
        self.assertEqual(fm.label("cargo test 2>&1 | tail -20", 300, "cargo-test"), "cargo-test")

    def test_unmatched_work_is_heavy_from_30s(self):
        self.assertEqual(fm.label("distrobox enter dev -- make", 30, None), "unmatched-heavy")
        self.assertEqual(fm.label("distrobox enter dev -- make", 29.9, None), "light")
        self.assertEqual(fm.label("git status", 2, None), "light")

    def test_unmatched_work_beside_a_wait_is_light(self):
        # The wait is the long part: `mu agent wait …; tail log` took 90 s.
        self.assertEqual(fm.label("mu agent wait r --timeout 90; git status", 90, None), "light")

    def test_quoted_operators_and_heredocs_dont_split(self):
        self.assertEqual(fm.segments("echo 'a; sleep 1' && make"), ["echo 'a; sleep 1'", "make"])
        cmd = "cat > x.sh <<'EOF'\nsleep 100\nEOF\nbash x.sh"
        self.assertEqual(fm.label(cmd, 100, None), "unmatched-heavy")

    def test_prefixes_are_skipped(self):
        self.assertEqual(fm.kind_of("timeout -k 5 600 sleep 10"), fm.WAIT)
        self.assertEqual(fm.kind_of("FOO=1 env BAR=2 time make"), fm.OTHER)
        self.assertIsNone(fm.kind_of("cd /x"))


class Naming(unittest.TestCase):
    def test_first_appearance_order_and_kept_names(self):
        n = fm.Namer("p", {"muzzle"})
        self.assertEqual([n(k) for k in ["x", "muzzle", "y", "x"]], ["p1", "muzzle", "p2", "p1"])

    def test_roles(self):
        self.assertEqual(fm.ROLES.get("main"), "orchestrator")
        self.assertEqual(fm.ROLES.get("rv"), "reviewer")
        self.assertIsNone(fm.ROLES.get("refuter"))  # other


class Leaks(unittest.TestCase):
    def test_each_pattern(self):
        users = {"someone"}
        for s, kind in [
            ("/tmp/x", "path"),
            ("a\\b", "path"),
            ("~", "home dir"),
            ("home/x", "home dir"),
            ("someone-dev", "home dir"),
            ("x@y.io", "email"),
            ("AKIAABCDEFGHIJKLMNOP", "token"),
            ("ghp_" + "a" * 36, "token"),
            ("sk-" + "A" * 20, "token"),
            ("deadbeef" * 4, "token"),
            ("otherhost.example", "hostname"),
        ]:
            with self.subTest(s=s):
                self.assertEqual(ct.leak(s, users), kind)

    def test_labels_are_clean(self):
        for s in ["linuxpc", "gate", "unmatched-heavy", "p12", "a3", "durationMs", "2026-10-08"]:
            with self.subTest(s=s):
                self.assertIsNone(ct.leak(s, {"someone"}))


if __name__ == "__main__":
    unittest.main()
