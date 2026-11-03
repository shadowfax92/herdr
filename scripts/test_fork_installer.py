"""Exercise the fork installer's download boundary without installing into the host.

The installer runs against a fake release: `curl` serves a fake herdr binary, a
shadowfax.tools tarball laid out by the real `herdr-tools/scripts/package.sh`,
and their SHA256SUMS. The fake herdr records its CLI calls and keeps a one-entry
plugin registry, so linking is observable without a real Herdr. Every install
goes into temp dirs (`HERDR_INSTALL_DIR`, `XDG_DATA_HOME`).
"""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[1]
INSTALLER = REPO / "distribution/fork-install.sh"
PACKAGE = REPO / "herdr-tools/scripts/package.sh"
RELEASES = "https://github.com/shadowfax92/herdr/releases"
TAG = "fork-v0.9.5-1"
PLUGIN_ID = "shadowfax.tools"

# Stands in for the installed herdr. `plugin list` prints the compact JSON shape
# real Herdr prints, with the registry's canonical plugin_root.
FAKE_HERDR = r"""#!/bin/sh
if [ -n "${TEST_HERDR_LOG:-}" ]; then printf '%s\n' "$*" >> "$TEST_HERDR_LOG"; fi
case "$1 ${2:-}" in
  "--version "*) printf 'herdr installer-test\n' ;;
  "plugin list")
    printf '{"id":"cli:plugin","result":{"type":"plugin_list","plugins":['
    if [ -s "$TEST_HERDR_REGISTRY" ]; then
      printf '{"plugin_id":"shadowfax.tools","name":"Herdr Tools","plugin_root":"%s","enabled":true}' \
        "$(cat "$TEST_HERDR_REGISTRY")"
    fi
    printf ']}}\n'
    ;;
  "plugin link")
    if [ "${TEST_LINK_FAIL:-}" = 1 ]; then echo 'link refused' >&2; exit 1; fi
    (cd "$3" && pwd -P) > "$TEST_HERDR_REGISTRY"
    printf '{"id":"cli:plugin","result":{"type":"plugin_linked"}}\n'
    ;;
  *) exit 2 ;;
esac
"""


@unittest.skipUnless(os.name == "posix", "installer requires a POSIX host")
class ForkInstallerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="herdr-fork-install-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.dest = self.root / "install with spaces"
        self.data_home = self.root / "data home"
        self.tools_dir = self.data_home / "herdr/plugins" / PLUGIN_ID
        self.payload = self.root / "payload"
        self.payload.write_text(FAKE_HERDR)
        self.digest = hashlib.sha256(self.payload.read_bytes()).hexdigest()
        self.tools_tarball = self.root / "tools.tar.gz"
        self.tools_digest = self.package_tools(self.tools_tarball)
        self.checksums = self.root / "SHA256SUMS"
        self.downloads = self.root / "downloads"
        self.herdr_log = self.root / "herdr-calls"
        self.registry = self.root / "registry"
        for command in ("awk", "cat", "chmod", "cp", "gzip", "mkdir", "mktemp",
                        "mv", "rm", "sed", "tar"):
            path = shutil.which(command)
            assert path is not None, command
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
        # unpacking, installation and the installed executables all run through
        # real tools.
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
  */herdr-tools-*.tar.gz)
    [ "${TEST_TOOLS_DOWNLOAD_FAIL:-}" != 1 ] || exit 22
    cp "$TEST_TOOLS_TARBALL" "$out"
    ;;
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

    def package_tools(self, tarball: Path, *, drop_binary: bool = False) -> str:
        """Lays out shadowfax.tools with package.sh, tars it as CI does, returns its hash."""
        work = Path(tempfile.mkdtemp(dir=self.root))
        binary = work / "herdr-tools"
        binary.write_text("#!/bin/sh\nprintf 'herdr-tools installer-test\\n'\n")
        binary.chmod(0o755)
        subprocess.run(["sh", str(PACKAGE), str(binary), str(work / "out")], check=True)
        if drop_binary:
            (work / "out" / PLUGIN_ID / "bin/herdr-tools").unlink()
        with tarfile.open(tarball, "w:gz") as archive:
            archive.add(work / "out" / PLUGIN_ID, arcname=PLUGIN_ID)
        return hashlib.sha256(tarball.read_bytes()).hexdigest()

    def run_installer(self, *, os_name="Linux", arch="x86_64", checksum=None,
                      tools_checksum=None, tool="shasum", **extra) -> subprocess.CompletedProcess[str]:
        """Runs the installer against the fake release.

        `checksum` replaces the whole SHA256SUMS; `tools_checksum` replaces only
        the tools line ("" drops it).
        """
        for name in ("sha256sum", "shasum", "openssl"):
            (self.bin / name).unlink(missing_ok=True)
        tool_path = shutil.which(tool)
        assert tool_path is not None, tool
        (self.bin / tool).symlink_to(tool_path)
        target_os = "macos" if os_name == "Darwin" else "linux"
        target_arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(arch, arch)
        asset = f"herdr-{target_os}-{target_arch}"
        if checksum is None:
            if tools_checksum is None:
                tools_checksum = f"{self.tools_digest}  herdr-tools-{target_os}-{target_arch}.tar.gz\n"
            checksum = f"{self.digest}  {asset}\n{tools_checksum}"
        self.checksums.write_text(checksum)
        return subprocess.run(
            ["/bin/sh", str(INSTALLER)], capture_output=True, text=True, check=False,
            env={**{k: v for k, v in os.environ.items() if not k.startswith("HERDR_")},
                 "PATH": str(self.bin),
                 "HERDR_INSTALL_DIR": str(self.dest), "XDG_DATA_HOME": str(self.data_home),
                 "TEST_OS": os_name, "TEST_ARCH": arch, "TEST_PAYLOAD": str(self.payload),
                 "TEST_TOOLS_TARBALL": str(self.tools_tarball),
                 "TEST_CHECKSUMS": str(self.checksums),
                 "TEST_DOWNLOADS": str(self.downloads),
                 "TEST_HERDR_LOG": str(self.herdr_log),
                 "TEST_HERDR_REGISTRY": str(self.registry),
                 "TEST_RELEASE_URL": f"{RELEASES}/tag/{TAG}", **extra},
        )

    def herdr_calls(self) -> list[str]:
        return self.herdr_log.read_text().splitlines() if self.herdr_log.exists() else []

    def make_old_tools_dir(self) -> Path:
        marker = self.tools_dir / "old-version"
        marker.parent.mkdir(parents=True)
        marker.write_text("old")
        return marker

    def assert_herdr_installed(self) -> None:
        installed = subprocess.check_output([str(self.dest / "herdr"), "--version"], text=True)
        self.assertEqual(installed.strip(), "herdr installer-test")

    def assert_says_how_to_rerun(self, result: subprocess.CompletedProcess[str]) -> None:
        rerun = f'{RELEASES}/latest/download/install.sh | HERDR_INSTALL_DIR="{self.dest}" sh'
        self.assertIn(f"curl -fsSL {rerun}", result.stderr)
        self.assertIn(f"HERDR_INSTALL_TOOLS=0 HERDR_INSTALL_DIR=", result.stderr)

    def assert_no_staging_left(self) -> None:
        plugins = self.data_home / "herdr/plugins"
        leftovers = [p.name for p in plugins.iterdir() if p.name != PLUGIN_ID] if plugins.exists() else []
        self.assertEqual(leftovers, [])

    def tests_that_an_interrupted_swap_restores_the_previous_plugin(self) -> None:
        marker = self.make_old_tools_dir()
        real_mv = shutil.which("mv")
        (self.bin / "mv").unlink()
        # Delivers SIGTERM to the installer at the worst moment: the old dir is
        # parked in the stage and the new one is about to move into place.
        self.write_command("mv", f"""#!/bin/sh
case "$1" in
  */.{PLUGIN_ID}.stage.*/{PLUGIN_ID}) kill -TERM "$PPID"; sleep 1; exit 1 ;;
esac
exec {real_mv} "$@"
""")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(marker.read_text(), "old", result.stdout + result.stderr)
        self.assert_no_staging_left()

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
                self.assert_herdr_installed()
                self.assertEqual(self.downloads.read_text().splitlines(), [
                    f"{RELEASES}/latest",
                    f"{RELEASES}/download/{TAG}/SHA256SUMS",
                    f"{RELEASES}/download/{TAG}/herdr-{target}",
                    f"{RELEASES}/download/{TAG}/herdr-tools-{target}.tar.gz",
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
                self.assertFalse(self.tools_dir.exists())

    def tests_that_failed_downloads_leave_no_install(self) -> None:
        result = self.run_installer(TEST_DOWNLOAD_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.dest / "herdr").exists())
        self.assertFalse(self.tools_dir.exists())

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

    def tests_that_tools_install_and_link_with_the_new_herdr(self) -> None:
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_herdr_installed()
        self.assertEqual(
            sorted(p.name for p in self.tools_dir.iterdir()),
            ["LICENSE", "README.md", "bin", "herdr-plugin.toml", "nvim"],
        )
        self.assertEqual((self.tools_dir / "herdr-plugin.toml").read_text(),
                         (REPO / "herdr-tools/herdr-plugin.toml").read_text())
        tools = subprocess.check_output([str(self.tools_dir / "bin/herdr-tools")], text=True)
        self.assertEqual(tools.strip(), "herdr-tools installer-test")
        self.assertEqual(self.herdr_calls(), [
            f"plugin list --plugin {PLUGIN_ID} --json",
            f"plugin link {self.tools_dir}",
        ])
        self.assertEqual(self.registry.read_text().strip(), str(self.tools_dir.resolve()))
        self.assert_no_staging_left()

    def tests_that_an_existing_link_from_the_same_dir_is_kept(self) -> None:
        marker = self.make_old_tools_dir()
        self.registry.write_text(f"{self.tools_dir.resolve()}\n")
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("already linked", result.stdout)
        self.assertEqual(self.herdr_calls(), [f"plugin list --plugin {PLUGIN_ID} --json"])
        # The files are still replaced; only the redundant link is skipped.
        self.assertFalse(marker.exists())
        self.assertTrue((self.tools_dir / "bin/herdr-tools").is_file())
        self.assert_no_staging_left()

    def tests_that_a_link_from_another_dir_is_replaced(self) -> None:
        elsewhere = self.root / "dev checkout"
        elsewhere.mkdir()
        self.registry.write_text(f"{elsewhere.resolve()}\n")
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"plugin link {self.tools_dir}", self.herdr_calls())
        self.assertEqual(self.registry.read_text().strip(), str(self.tools_dir.resolve()))

    def tests_that_tools_can_be_skipped(self) -> None:
        # No tools line in SHA256SUMS: the skip must not even look for one.
        result = self.run_installer(tools_checksum="", HERDR_INSTALL_TOOLS="0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_herdr_installed()
        self.assertIn("HERDR_INSTALL_TOOLS=0", result.stdout)
        self.assertFalse((self.data_home / "herdr").exists())
        self.assertEqual(self.herdr_calls(), [])
        self.assertFalse(any("herdr-tools" in url for url in self.downloads.read_text().splitlines()))

    def tests_that_a_bad_tools_checksum_keeps_the_old_plugin_dir(self) -> None:
        marker = self.make_old_tools_dir()
        asset = "herdr-tools-linux-x86_64.tar.gz"
        for tools_checksum in (
            f"{'0' * 64}  {asset}\n",
            f"{'g' * 64}  {asset}\n",
            f"{self.tools_digest}  {asset}\n" * 2,
        ):
            with self.subTest(tools_checksum=tools_checksum):
                (self.dest / "herdr").unlink(missing_ok=True)
                result = self.run_installer(tools_checksum=tools_checksum)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("checksum", result.stderr.lower())
                self.assert_herdr_installed()
                self.assert_says_how_to_rerun(result)
                self.assertEqual(marker.read_text(), "old")
                self.assertEqual(sorted(p.name for p in self.tools_dir.iterdir()), ["old-version"])
                self.assertEqual(self.herdr_calls(), [])
                self.assert_no_staging_left()

    def tests_that_a_missing_tools_tarball_fails_after_herdr_installs(self) -> None:
        for name, params in (
            ("not in SHA256SUMS", {"tools_checksum": ""}),
            ("not downloadable", {"TEST_TOOLS_DOWNLOAD_FAIL": "1"}),
        ):
            with self.subTest(name):
                (self.dest / "herdr").unlink(missing_ok=True)
                result = self.run_installer(**params)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("herdr-tools-linux-x86_64.tar.gz", result.stderr)
                self.assert_herdr_installed()
                self.assert_says_how_to_rerun(result)
                self.assertFalse(self.tools_dir.exists())
                self.assertEqual(self.herdr_calls(), [])
                self.assert_no_staging_left()

    def tests_that_a_tarball_without_the_binary_keeps_the_old_plugin_dir(self) -> None:
        marker = self.make_old_tools_dir()
        self.tools_digest = self.package_tools(self.tools_tarball, drop_binary=True)
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a shadowfax.tools plugin directory", result.stderr)
        self.assert_herdr_installed()
        self.assertEqual(marker.read_text(), "old")
        self.assert_no_staging_left()

    def tests_that_a_failed_link_keeps_herdr_and_says_how_to_rerun(self) -> None:
        result = self.run_installer(TEST_LINK_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("plugin link", result.stderr)
        self.assert_herdr_installed()
        self.assert_says_how_to_rerun(result)
        # The new files are in place; a rerun only has to link them.
        self.assertTrue((self.tools_dir / "bin/herdr-tools").is_file())


if __name__ == "__main__":
    unittest.main()
