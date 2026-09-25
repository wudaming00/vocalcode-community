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

    def test_release_notes_are_the_body_of_this_one_product(self):
        # community-release.yml publishes this file word for word.
        text = (release.ROOT / release.RELEASE_NOTES).read_text(encoding="utf-8")
        self.assertEqual(release.release_notes_problems(text), [])
        self.assertTrue(text.startswith("# VocalCode\n"))
        for name in ("`VocalCodeSetup.exe`", "`VocalCode-<version>.dmg`", "`VocalCode-source-<version>.tar.gz`"):
            self.assertIn(name, text)
        self.assertIn("scoop uninstall vocalcode", text)
        workflow = (release.ROOT / ".github/workflows/community-release.yml").read_text(encoding="utf-8")
        self.assertIn("--notes-file packaging/community/RELEASE-NOTES.md", workflow)
        self.assertIn("release_notes_problems(", workflow)

    def test_stale_or_misnamed_release_notes_are_refused(self):
        good = (release.ROOT / release.RELEASE_NOTES).read_text(encoding="utf-8")
        stale = [
            good.replace("# VocalCode\n", "# VocalCode Community\n", 1),
            good.replace("`VocalCodeSetup.exe`", "`VocalCodeCommunitySetup.exe`"),
            good.replace("`VocalCode-<version>.dmg`", "`VocalCodeCommunity-<version>.dmg`"),
            good.replace("`VocalCode-source-<version>.tar.gz`", "the attached source archive"),
            good + "\nCommunity installs separately from the previous paid edition.\n",
            good + "\nVocalCode Community updates use GitHub Releases.\n",
            good + "\n社区版独立安装，不覆盖旧版。\n",
            good.replace("## New in ", "## Changes in "),
        ]
        for text in stale:
            self.assertNotEqual(release.release_notes_problems(text), [], text[:80])
        # The early builds may be named, across a line break too.
        for text in (good + "\nVocalCode\nCommunity 1.3.1 and 1.4.0 were separate.\n",
                     good + "\n早期社区版 1.3.1 和 1.4.0。\n"):
            self.assertEqual(release.release_notes_problems(text), [])
        # A release needs its own section: the previous one's notes are refused.
        self.assertEqual(release.release_notes_problems("# VocalCode\n\n## New in 1.4.1\n" + good.split("\n", 1)[1], "1.4.1"), [])
        self.assertNotEqual(release.release_notes_problems(good, "9.9.9"), [])

    def test_packaging_accepts_only_the_installed_identity(self):
        release_build = {"edition": "community", "identity": "release", "data_directory": "VocalCode"}
        self.assertTrue(release.has_release_identity(release_build))
        for change in ({"identity": "development"}, {"data_directory": "VocalCode Dev"}, {"identity": None}):
            self.assertFalse(release.has_release_identity({**release_build, **change}))
        self.assertFalse(release.has_release_identity({"edition": "community", "data_directory": "VocalCode"}))
        # The binaries the signed release packages come from this CI step.
        ci = (release.ROOT / ".github/workflows/community-ci.yml").read_text(encoding="utf-8")
        step = ci.split("- name: Unsigned community build", 1)[1].split("- name:", 1)[0]
        self.assertIn("VOCALCODE_RELEASE_IDENTITY: '1'", step)
        script = (release.ROOT / "packaging/community/windows.ps1").read_text(encoding="utf-8")
        self.assertIn("$info.identity -ne 'release'", script)
        self.assertEqual(script.count("$info.data_directory -ne 'VocalCode'"), 2)

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
        self.assertIn('Name: "{app}\\cargs.dll"', install_delete)
        self.assertIn('Name: "{app}\\THIRD-PARTY-LICENSES"', install_delete)
        # An updater that would restart a copy other than {app}\VocalCode.exe
        # (a Scoop installation, which registers nothing) is refused before
        # anything else happens, so the silent run exits with code 7.
        prepare = text.split("function PrepareToInstall(", 1)[1].split("\nend;", 1)[0]
        body = [line.strip() for line in prepare.split("begin\n", 1)[1].splitlines()]
        self.assertEqual(body[:3], ["Result := UpdaterRestartsAnotherCopy();", "if Result <> '' then", "exit;"])
        check = text.split("function UpdaterRestartsAnotherCopy(", 1)[1].split("\nend;", 1)[0]
        self.assertIn("GetEnv('VC_UPDATE_EXE')", check)
        self.assertIn("AddBackslash(ExpandConstant('{app}')) + '{#AppExe}'", check)
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
