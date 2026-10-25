"""Exercise the fork installer's download boundary without installing into the host."""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "distribution/fork-install.sh"
RELEASES = "https://github.com/shadowfax92/herdr/releases"
TAG = "fork-v0.9.5-1"


@unittest.skipUnless(os.name == "posix", "installer requires a POSIX host")
class ForkInstallerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="herdr-fork-install-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.dest = self.root / "install with spaces"
        self.payload = self.root / "payload"
        self.payload.write_text("#!/bin/sh\nprintf 'herdr installer-test\\n'\n")
        self.digest = hashlib.sha256(self.payload.read_bytes()).hexdigest()
        self.checksums = self.root / "SHA256SUMS"
        self.downloads = self.root / "downloads"
        for command in ("awk", "chmod", "cp", "mkdir", "mktemp", "mv", "rm"):
            path = shutil.which(command)
            self.assertIsNotNone(path, command)
            (self.bin / command).symlink_to(path)
        self.write_command("uname", """#!/bin/sh
case "$1" in
  -s) echo "${TEST_OS:-Linux}" ;;
  -m) echo "${TEST_ARCH:-x86_64}" ;;
  -o) echo "${TEST_KERNEL:-GNU/Linux}" ;;
  *) exit 1 ;;
esac
""")
        # Only HTTP and platform detection are replaced. Hashing, permissions,
        # installation and the installed executable all run through real tools.
        self.write_command("curl", """#!/bin/sh
out=''
previous=''
for argument in "$@"; do
  if [ "$previous" = '-o' ]; then out="$argument"; fi
  previous="$argument"
  case "$argument" in https://*) url="$argument" ;; esac
done
printf '%s\\n' "$url" >> "$TEST_DOWNLOADS"
case "$url" in
  */releases/latest) printf '%s' "$TEST_RELEASE_URL" ;;
  */SHA256SUMS) cp "$TEST_CHECKSUMS" "$out" ;;
  */herdr-*)
    [ "${TEST_DOWNLOAD_FAIL:-}" != 1 ] || exit 22
    cp "$TEST_PAYLOAD" "$out"
    ;;
  *) exit 22 ;;
esac
""")

    def write_command(self, name: str, body: str) -> None:
        path = self.bin / name
        path.write_text(body)
        path.chmod(0o755)

    def run_installer(self, *, os_name="Linux", arch="x86_64", checksum=None,
                      tool="shasum", **extra) -> subprocess.CompletedProcess[str]:
        for name in ("sha256sum", "shasum", "openssl"):
            (self.bin / name).unlink(missing_ok=True)
        tool_path = shutil.which(tool)
        self.assertIsNotNone(tool_path, tool)
        (self.bin / tool).symlink_to(tool_path)
        target_os = "macos" if os_name == "Darwin" else "linux"
        target_arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(arch, arch)
        asset = f"herdr-{target_os}-{target_arch}"
        self.checksums.write_text(
            checksum if checksum is not None else f"{self.digest}  {asset}\n"
        )
        return subprocess.run(
            ["/bin/sh", str(INSTALLER)], capture_output=True, text=True, check=False,
            env={**os.environ, "PATH": str(self.bin),
                 "HERDR_INSTALL_DIR": str(self.dest), "TEST_OS": os_name,
                 "TEST_ARCH": arch, "TEST_PAYLOAD": str(self.payload),
                 "TEST_CHECKSUMS": str(self.checksums),
                 "TEST_DOWNLOADS": str(self.downloads),
                 "TEST_RELEASE_URL": f"{RELEASES}/tag/{TAG}", **extra},
        )

    def tests_that_four_platforms_install_from_one_pinned_release(self) -> None:
        for os_name, arch, target in (
            ("Linux", "x86_64", "linux-x86_64"),
            ("Linux", "aarch64", "linux-aarch64"),
            ("Darwin", "amd64", "macos-x86_64"),
            ("Darwin", "arm64", "macos-aarch64"),
        ):
            with self.subTest(target=target):
                self.downloads.unlink(missing_ok=True)
                result = self.run_installer(os_name=os_name, arch=arch)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                installed = subprocess.check_output([str(self.dest / "herdr"), "--version"], text=True)
                self.assertEqual(installed.strip(), "herdr installer-test")
                self.assertEqual(self.downloads.read_text().splitlines(), [
                    f"{RELEASES}/latest",
                    f"{RELEASES}/download/{TAG}/SHA256SUMS",
                    f"{RELEASES}/download/{TAG}/herdr-{target}",
                ])

    def tests_that_all_checksum_tools_work(self) -> None:
        for tool in ("sha256sum", "shasum", "openssl"):
            with self.subTest(tool=tool):
                result = self.run_installer(tool=tool)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def tests_that_invalid_checksums_preserve_an_existing_install(self) -> None:
        self.dest.mkdir()
        binary = self.dest / "herdr"
        binary.write_text("existing installation")
        for checksum in (
            f"{'0' * 64}  herdr-linux-x86_64\n",
            f"{'g' * 64}  herdr-linux-x86_64\n",
            f"{self.digest}  another-binary\n",
            f"{self.digest}  herdr-linux-x86_64\n" * 2,
        ):
            with self.subTest(checksum=checksum):
                result = self.run_installer(checksum=checksum)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("checksum", result.stderr.lower())
                self.assertEqual(binary.read_text(), "existing installation")

    def tests_that_failed_downloads_leave_no_install(self) -> None:
        result = self.run_installer(TEST_DOWNLOAD_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.dest / "herdr").exists())

    def tests_that_unsupported_platforms_fail_before_downloading(self) -> None:
        for params in ({"os_name": "FreeBSD"}, {"arch": "riscv64"}, {"TEST_KERNEL": "Android"}):
            with self.subTest(params=params):
                result = self.run_installer(**params)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("unsupported", result.stderr.lower())
                self.assertFalse(self.downloads.exists())

    def tests_that_latest_cannot_resolve_to_upstream(self) -> None:
        result = self.run_installer(TEST_RELEASE_URL="https://github.com/herdrdev/herdr/releases/tag/v9.9.9")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.dest / "herdr").exists())
        self.assertEqual(self.downloads.read_text().splitlines(), [f"{RELEASES}/latest"])


if __name__ == "__main__":
    unittest.main()
