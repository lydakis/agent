from pathlib import Path
import shutil
import tempfile
import unittest

import stamp_version


ROOT = Path(__file__).resolve().parents[1]
FILES = ["Cargo.toml", "Cargo.lock", "client/Cargo.toml", "sonny/Cargo.toml"]


class StampVersionTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in FILES:
            (self.root / name).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(ROOT / name, self.root / name)

    def read(self):
        return {name: (self.root / name).read_text() for name in FILES}

    def test_main_says_0_0_0(self):
        # Every manifest of ours on main, and its lock entry: a build from
        # main never passes for a release.
        for manifest, name in stamp_version.PACKAGES.items():
            text = (ROOT / manifest).read_text()
            self.assertIn(f'[package]\nname = "{name}"\nversion = "0.0.0"\n', text)
            self.assertIn(f'[[package]]\nname = "{name}"\nversion = "0.0.0"\n',
                          (ROOT / "Cargo.lock").read_text())

    def test_stamps_each_manifest_and_lock_entry_and_nothing_else(self):
        before = self.read()
        stamp_version.stamp(self.root, "1.2.3-rc.1")
        after = self.read()
        for name in FILES:
            old, new = before[name].splitlines(), after[name].splitlines()
            self.assertEqual(len(old), len(new))
            changed = [(a, b) for a, b in zip(old, new) if a != b]
            expected = 3 if name == "Cargo.lock" else 1
            self.assertEqual(changed, [('version = "0.0.0"', 'version = "1.2.3-rc.1"')] * expected,
                             name)

    def test_refuses_a_tree_already_stamped_and_writes_nothing(self):
        stamp_version.stamp(self.root, "1.2.3")
        stamped = self.read()
        with self.assertRaisesRegex(ValueError, "agent-runtime has version 1.2.3, not main's 0.0.0"):
            stamp_version.stamp(self.root, "1.2.4")
        self.assertEqual(self.read(), stamped)

    def test_refuses_a_lock_out_of_step_and_writes_nothing(self):
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace(
            'name = "sonny"\nversion = "0.0.0"', 'name = "sonny"\nversion = "0.9.0"'))
        before = self.read()
        with self.assertRaisesRegex(ValueError, "expected one sonny 0.0.0"):
            stamp_version.stamp(self.root, "1.2.3")
        self.assertEqual(self.read(), before)

    def test_refuses_what_is_no_release_version(self):
        for version in ["0.0.0", "v1.2.3", "1.2", "01.2.3", "1.2.3;x", ""]:
            with self.subTest(version=version):
                with self.assertRaises(ValueError):
                    stamp_version.stamp(self.root, version)


if __name__ == "__main__":
    unittest.main()
