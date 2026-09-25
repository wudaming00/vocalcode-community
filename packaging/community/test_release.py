import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import release


class ReleaseTests(unittest.TestCase):
    def test_version_and_package_identity(self):
        # The identity a paid VocalCode's updater accepts: its bundle, its
        # bundle identifier, the executable it relaunches and the file names
        # it downloads.
        self.assertRegex(release.version(), r"^\d+\.\d+\.\d+$")
        self.assertEqual(release.BUNDLE_ID, "app.vocalcode.VocalCode")
        self.assertEqual(release.APP, "VocalCode.app")
        self.assertEqual(release.EXECUTABLE, "VocalCode")
        self.assertEqual(release.WINDOWS_INSTALLER, "VocalCodeSetup.exe")
        self.assertEqual(release.dmg_name("1.4.1"), "VocalCode-1.4.1.dmg")

    def test_gatekeeper_and_signature_readings_match_the_paid_updater(self):
        accepted = ("/Volumes/VocalCode/VocalCode.app: accepted\n"
                    "source=Notarized Developer ID\norigin=Developer ID Application: Daming Wu (58Y98W3QQK)\n")
        self.assertTrue(release.gatekeeper_accepted(accepted))
        self.assertFalse(release.gatekeeper_accepted(accepted.replace("Notarized ", "")))
        self.assertFalse(release.gatekeeper_accepted(accepted.replace("accepted", "rejected")))
        detail = "Executable=/x\nIdentifier=app.vocalcode.VocalCode\nTeamIdentifier=58Y98W3QQK\n"
        self.assertEqual(release.team_of(detail), release.TEAM)
        self.assertEqual(release.identifier_of(detail), release.BUNDLE_ID)
        self.assertIsNone(release.team_of("TeamIdentifier missing\n"))

    def test_file_record_binds_bytes_and_size(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "fixture.exe"
            path.write_bytes(b"synthetic fixture, not executable")
            record = release.file_record(path)
            self.assertEqual(record["sha256"], hashlib.sha256(path.read_bytes()).hexdigest())
            self.assertEqual(record["size"], path.stat().st_size)
            path.write_bytes(b"")
            with self.assertRaises(ValueError):
                release.file_record(path)

    def test_universal_runtime_is_thinned_and_reverified(self):
        source, destination = Path("upstream.dylib"), Path("bundle.dylib")
        with patch.object(release, "run", side_effect=["x86_64 arm64\n", "", "arm64\n"]) as runner:
            release.copy_arm64(source, destination)
        self.assertEqual(runner.call_args_list[1].args, ("/usr/bin/lipo", source, "-thin", "arm64", "-output", destination))
        with patch.object(release, "run", side_effect=["x86_64 arm64\n", "", "x86_64 arm64\n"]):
            with self.assertRaises(ValueError):
                release.copy_arm64(source, destination)

    def test_runtime_without_approved_arm64_slice_fails_closed(self):
        for architecture in ("x86_64", "", "arm64 armv7"):
            with patch.object(release, "run", return_value=architecture):
                with self.assertRaises(ValueError):
                    release.copy_arm64(Path("upstream.dylib"), Path("bundle.dylib"))

    def test_missing_and_oversized_assets_are_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "fixture"
            with self.assertRaises(ValueError):
                release.file_record(path)
            with path.open("wb") as stream:
                stream.truncate(512 * 1024 * 1024 + 1)
            with self.assertRaises(ValueError):
                release.file_record(path)

    def test_manifest_matches_exact_versioned_assets_and_source(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "Cargo.toml").write_text('[workspace.package]\nversion="1.3.1"\n', encoding="utf-8")
            output = root / "dist-community/artifacts"
            output.mkdir(parents=True)
            for name in ("VocalCodeSetup.exe", "VocalCode-1.3.1.dmg"):
                (output / name).write_bytes(name.encode())
            def fake_run(*args):
                if args[:3] == ("git", "rev-parse", "HEAD"):
                    return "1" * 40
                destination = next(a[len("--output="):] for a in args if str(a).startswith("--output="))
                Path(destination).write_bytes(b"synthetic source archive")
                return ""
            with patch.object(release, "ROOT", root), patch.object(release, "run", fake_run):
                release.manifest()
            data = json.loads((output / "latest.json").read_text())
            self.assertEqual(data["channel"], "community-stable")
            self.assertEqual(data["schema"], "vocalcode-community-update-v1")
            self.assertEqual(data["source_commit"], "1" * 40)
            self.assertEqual(data["windows"]["url"],
                             "https://github.com/wudaming00/vocalcode-community/releases/download/v1.3.1/VocalCodeSetup.exe")
            self.assertEqual(data["macos"]["url"],
                             "https://github.com/wudaming00/vocalcode-community/releases/download/v1.3.1/VocalCode-1.3.1.dmg")
            for platform in ("windows", "macos"):
                self.assertEqual(data[platform]["version"], "1.3.1")
                self.assertRegex(data[platform]["sha256"], r"^[0-9a-f]{64}$")
            self.assertNotIn("Community", data["notes"])
            self.assertIn("latest.json", (output / "SHA256SUMS").read_text())
            self.assertTrue((output / "VocalCode-source-1.3.1.tar.gz").is_file())

    def test_release_workflow_fails_closed_and_limits_credentials(self):
        workflow = (release.ROOT / ".github/workflows/community-release.yml").read_text(encoding="utf-8")
        self.assertNotIn("pull_request_target", workflow)
        self.assertNotIn("self-hosted", workflow)
        self.assertIn("github.actor == 'wudaming00'", workflow)
        self.assertIn("r['conclusion'] == 'success'", workflow)
        self.assertIn("run-id: ${{ needs.preflight.outputs.ci_run }}", workflow)
        self.assertIn("name: vocalcode-community-dev-${{ matrix.os }}-${{ github.sha }}", workflow)
        self.assertIn("environment: community-release", workflow)
        self.assertIn("needs: [preflight, verify]", workflow)
        self.assertIn("--draft=false --latest", workflow)
        self.assertNotIn("--clobber", workflow)
        build = workflow.split("  build:\n", 1)[1].split("  sign-windows:\n", 1)[0]
        verify = workflow.split("  verify:\n", 1)[1].split("  publish:\n", 1)[0]
        self.assertNotIn("secrets.", build)
        self.assertNotIn("secrets.", verify)
        for line in workflow.splitlines():
            if "uses:" in line:
                self.assertRegex(line, r"@[a-f0-9]{40}$")

    def test_installer_takes_over_the_paid_installation_and_keeps_user_data(self):
        text = (release.ROOT / "packaging/community/windows.iss").read_text(encoding="utf-8")
        # Inno Setup names the uninstall key "<AppId>_is1"; the paid releases
        # had no AppId, so theirs came from AppName "VocalCode".
        self.assertIn('#define AppName "VocalCode"', text)
        self.assertIn('#define AppExe "VocalCode.exe"', text)
        self.assertIn("\nAppId=VocalCode\n", text)
        self.assertIn("\nDefaultDirName={autopf}\\VocalCode\n", text)
        self.assertIn("\nPrivilegesRequired=lowest\n", text)
        self.assertIn("\nAppMutex=Local\\VocalCode.Desktop\n", text)
        self.assertIn("\nVersionInfoProductName={#AppName}\n", text)
        self.assertIn("\nVersionInfoOriginalFileName=VocalCodeSetup.exe\n", text)
        self.assertIn("\nOutputBaseFilename=VocalCodeSetup\n", text)
        # The paid uninstall log lists the data folder's first vocalcode.toml.
        self.assertIn("\nUninstallLogMode=overwrite\n", text)
        self.assertIn("SignedUninstaller=yes", text)
        self.assertIn("skipifsilent", text)
        self.assertNotIn("[UninstallDelete]", text)
        self.assertNotIn("--uninstall-cleanup", text)
        self.assertNotIn("DelTree", text)
        install_delete = text.split("[InstallDelete]\n", 1)[1].split("\n[", 1)[0]
        for line in install_delete.splitlines():
            if line.startswith("Type:"):
                self.assertIn('Name: "{app}\\', line)
        # The early free build is replaced by running its own uninstaller,
        # which keeps its data folder for the import.
        self.assertIn("VocalCode.Community_is1", text)
        self.assertIn("Local\\VocalCode.Community.Desktop", text)
        self.assertNotIn("AppData", text)

    def test_inno_resource_padding_and_e32_signing_handoff(self):
        script = (release.ROOT / "packaging/community/windows.ps1").read_text(encoding="utf-8")
        for field in ("ProductName", "OriginalFilename", "ProductVersion"):
            self.assertIn("$v." + field + ".Trim()", script)
        self.assertIn(r"^uninst-6\.7\.3-[a-f0-9]{10}\.e32$", script)
        self.assertIn("$files.Count -ne 1", script)
        self.assertIn("[version]$_.Name", script)
        self.assertIn(r"^CN=Pyrsys B\.V\., O=Pyrsys B\.V\., S=Noord-Holland, C=NL$", script)
        self.assertIn("9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732", script)
        workflow = (release.ROOT / ".github/workflows/community-release.yml").read_text(encoding="utf-8")
        self.assertIn("files: ${{ steps.uninstaller.outputs.path }}", workflow)


if __name__ == "__main__":
    unittest.main()
