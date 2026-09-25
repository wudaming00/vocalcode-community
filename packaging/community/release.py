"""VocalCode packaging/release helpers. No commerce or production-site access.

The free build ships under the identity the paid releases had (bundle
VocalCode.app, app.vocalcode.VocalCode, VocalCodeSetup.exe), so a paid
installation's updater can replace it in place. What that updater checks is
checked here before anything is published.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import secrets
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = "wudaming00/vocalcode-community"
TEAM = "58Y98W3QQK"
APP = "VocalCode.app"
BUNDLE_ID = "app.vocalcode.VocalCode"
# The paid app relaunches Contents/MacOS/VocalCode after swapping bundles.
EXECUTABLE = "VocalCode"
WINDOWS_INSTALLER = "VocalCodeSetup.exe"


def dmg_name(value: str) -> str:
    return f"VocalCode-{value}.dmg"


def source_archive_name(value: str) -> str:
    return f"VocalCode-source-{value}.tar.gz"


# Published word for word as the GitHub release body (community-release.yml).
RELEASE_NOTES = Path("packaging/community/RELEASE-NOTES.md")


def release_notes_problems(text: str, current: str | None = None) -> list[str]:
    """Why RELEASE-NOTES.md cannot be this release's body, or [] when it can.

    One product, VocalCode: the title, the three asset names the release
    attaches, and "Community" (or 社区版) only where the early free builds,
    VocalCode Community 1.3.1 and 1.4.0, are named. With `current`, the notes
    must also have that version's "## New in" section, so a release cannot
    ship the previous release's notes."""
    problems = []
    lines = text.splitlines()
    if not lines or lines[0] != "# VocalCode":
        problems.append('the title must be "# VocalCode"')
    for name in (WINDOWS_INSTALLER, dmg_name("<version>"), source_archive_name("<version>")):
        if f"`{name}`" not in text:
            problems.append(f"the downloads must name `{name}`")
    for legacy in ("VocalCodeCommunitySetup", "VocalCodeCommunity-"):
        if legacy in text:
            problems.append(f"{legacy} is an early build's asset, not this release's")
    flat = " ".join(text.split())
    for match in re.finditer(r"Community|社区版", flat):
        before, after = flat[:match.start()], flat[match.end():]
        early = (before.endswith("VocalCode ") and after.startswith(" 1.3.1")) if match.group() == "Community" \
            else "1.3.1" in after[:8]
        if not early:
            problems.append(f"'{match.group()}' names something other than the early builds 1.3.1 and 1.4.0: "
                            f"…{flat[max(0, match.start() - 30):match.end() + 30]}…")
    if not re.search(r"^## New in \d+\.\d+\.\d+$", text, re.MULTILINE):
        problems.append('the notes need a "## New in <version>" section')
    if current is not None and f"## New in {current}" not in lines:
        problems.append(f'the notes have no "## New in {current}" section for this release')
    return problems


RUNTIMES = ("libonnxruntime.dylib", "libsherpa-onnx-c-api.dylib", "libsherpa-onnx-cxx-api.dylib")


def version() -> str:
    value = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", value):
        raise ValueError("stable versions must be canonical X.Y.Z")
    return value


def has_release_identity(build_info: dict) -> bool:
    """Only a binary built with VOCALCODE_RELEASE_IDENTITY=1 uses the installed
    app's data folder, login item and bundle identifier (community.rs)."""
    return build_info.get("identity") == "release" and build_info.get("data_directory") == "VocalCode"


def run(*args: str | Path, timeout: int = 180, env=None) -> str:
    # Never put a credential-bearing command or raw subprocess output in an
    # exception/log. Codesign/notary passphrase arguments are confidential.
    try:
        result = subprocess.run([str(x) for x in args], capture_output=True, text=True, timeout=timeout, env=env)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"{Path(args[0]).name} exceeded its deadline") from None
    if result.returncode:
        raise RuntimeError(f"{Path(args[0]).name} failed (exit {result.returncode})")
    return result.stdout


def copy_notices(destination: Path) -> None:
    for name in ("LICENSE", "LICENSING.md", "BUILDING.md", "THIRD-PARTY-NOTICES.txt"):
        shutil.copy2(ROOT / name, destination / name)
    shutil.copytree(ROOT / "THIRD-PARTY-LICENSES", destination / "THIRD-PARTY-LICENSES")


def file_record(path: Path) -> dict:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"not an ordinary release file: {path.name}")
    size = path.stat().st_size
    if not 0 < size <= 512 * 1024 * 1024:
        raise ValueError("release file exceeds the updater's size limit")
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"size": size, "sha256": digest}


def copy_arm64(source: Path, destination: Path) -> None:
    # Upstream ONNX/sherpa releases are universal2. Keep the reviewed arm64
    # slice for this arm64-only product, then sign that exact bundled copy.
    architectures = set(run("/usr/bin/lipo", "-archs", source).split())
    if "arm64" not in architectures or not architectures <= {"arm64", "x86_64"}:
        raise ValueError(f"unsupported native runtime architecture: {source.name}")
    if architectures == {"arm64"}:
        shutil.copy2(source, destination)
    else:
        run("/usr/bin/lipo", source, "-thin", "arm64", "-output", destination)
    if run("/usr/bin/lipo", "-archs", destination).strip() != "arm64":
        raise ValueError(f"bundled runtime is not arm64-only: {source.name}")


def build_macos() -> None:
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "release"
    volume = ROOT / "dist-community" / "macos"
    contents = volume / APP / "Contents"
    contents.mkdir(parents=True, exist_ok=False)
    for name in ("MacOS", "Frameworks", "Resources"):
        (contents / name).mkdir()
    shutil.copy2(target / "vocalcode-app", contents / "MacOS" / EXECUTABLE)
    (contents / "MacOS" / EXECUTABLE).chmod(0o755)
    for name in RUNTIMES:
        copy_arm64(target / name, contents / "Frameworks" / name)
    data = plistlib.loads((ROOT / "packaging/macos/Info.plist").read_bytes())
    data.update(CFBundleName="VocalCode", CFBundleDisplayName="VocalCode", CFBundleExecutable=EXECUTABLE,
                CFBundleIdentifier=BUNDLE_ID, CFBundleShortVersionString=version(), CFBundleVersion=version())
    (contents / "Info.plist").write_bytes(plistlib.dumps(data))
    run("python3", ROOT / "packaging/macos/make_icon.py", contents / "Resources/VocalCode.icns", timeout=300)
    copy_notices(contents / "Resources")
    (volume / "Applications").symlink_to("/Applications", target_is_directory=True)
    info = json.loads(run(contents / "MacOS" / EXECUTABLE, "--build-info"))
    if info.get("edition") != "community" or info.get("version") != version():
        raise ValueError("bundle is not the expected runnable free build")
    if not has_release_identity(info):
        raise ValueError("bundle holds a development build; build it with VOCALCODE_RELEASE_IDENTITY=1")
    run("/usr/bin/ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", volume, ROOT / "dist-community/macos-unsigned.zip")


def notarize(path: Path, key: Path, key_id: str, issuer: str) -> None:
    result = json.loads(run("/usr/bin/xcrun", "notarytool", "submit", path, "--key", key,
                            "--key-id", key_id, "--issuer", issuer, "--wait", "--timeout", "30m",
                            "--output-format", "json", timeout=1900))
    if result.get("status") != "Accepted":
        raise RuntimeError("Apple did not accept the notarization submission")


def sign_macos() -> None:
    temp_root = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    work = Path(tempfile.mkdtemp(prefix="vocalcode-signing-", dir=temp_root))
    work.chmod(0o700)
    keychain = work / "signing.keychain-db"
    password = secrets.token_urlsafe(36)
    print("::add-mask::" + password)
    p12 = work / "certificate.p12"
    notary_key = work / "AuthKey.p8"
    prior = run("/usr/bin/security", "list-keychains", "-d", "user")
    prior_paths = re.findall(r'"([^"\n]+)"', prior)
    try:
        p12.write_bytes(base64.b64decode(os.environ.pop("MACOS_CERT_P12"), validate=True))
        notary_key.write_bytes(base64.b64decode(os.environ.pop("NOTARY_KEY_P8"), validate=True))
        p12.chmod(0o600)
        notary_key.chmod(0o600)
        cert_password = os.environ.pop("MACOS_CERT_PASSWORD")
        issuer = os.environ.pop("NOTARY_ISSUER")
        key_id = os.environ.pop("NOTARY_KEY_ID")
        run("/usr/bin/security", "create-keychain", "-p", password, keychain)
        run("/usr/bin/security", "set-keychain-settings", "-lut", "21600", keychain)
        run("/usr/bin/security", "unlock-keychain", "-p", password, keychain)
        run("/usr/bin/security", "import", p12, "-k", keychain, "-P", cert_password,
            "-T", "/usr/bin/codesign", "-T", "/usr/bin/security")
        run("/usr/bin/security", "set-key-partition-list", "-S", "apple-tool:,apple:,codesign:", "-s", "-k", password, keychain)
        run("/usr/bin/security", "list-keychains", "-d", "user", "-s", keychain, *prior_paths)
        identities = run("/usr/bin/security", "find-identity", "-v", "-p", "codesigning", keychain)
        matches = re.findall(r'([0-9A-F]{40}) "Developer ID Application: [^"\n]+ \(' + TEAM + r'\)"', identities)
        if len(matches) != 1:
            raise RuntimeError("expected exactly one approved Developer ID identity")
        identity = matches[0]
        volume = ROOT / "dist-community/macos"
        app = volume / APP
        sign = ("/usr/bin/codesign", "--force", "--options", "runtime", "--timestamp", "--sign", identity, "--keychain", keychain)
        for name in RUNTIMES:
            run(*sign, app / "Contents/Frameworks" / name)
        run(*sign, "--entitlements", ROOT / "packaging/macos/VocalCode.entitlements", app)
        run("/usr/bin/codesign", "--verify", "--deep", "--strict", app)
        archive = work / "app.zip"
        run("/usr/bin/ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", app, archive)
        notarize(archive, notary_key, key_id, issuer)
        run("/usr/bin/xcrun", "stapler", "staple", app)
        run("/usr/bin/xcrun", "stapler", "validate", app)
        output = ROOT / "dist-community/artifacts"
        output.mkdir(parents=True, exist_ok=True)
        dmg = output / dmg_name(version())
        if dmg.exists():
            raise ValueError("refusing to overwrite a release disk image")
        run("/usr/bin/hdiutil", "create", "-volname", "VocalCode", "-srcfolder", volume,
            "-format", "UDZO", dmg, timeout=300)
        run(*sign, dmg)
        notarize(dmg, notary_key, key_id, issuer)
        run("/usr/bin/xcrun", "stapler", "staple", dmg)
        run("/usr/bin/xcrun", "stapler", "validate", dmg)
        print("Signed and notarized app and disk image; no installer executed with credentials.")
    finally:
        try:
            run("/usr/bin/security", "list-keychains", "-d", "user", "-s", *prior_paths)
        finally:
            try:
                if keychain.exists():
                    run("/usr/bin/security", "delete-keychain", keychain)
            finally:
                if work.resolve().parent != temp_root or work.is_symlink():
                    raise RuntimeError("refusing unsafe signing cleanup")
                shutil.rmtree(work)


def gatekeeper_accepted(text: str) -> bool:
    """The paid updater's reading of an spctl verdict (read_gatekeeper_verdict)."""
    return any(line.strip().endswith(": accepted") for line in text.splitlines()) and \
        "source=Notarized Developer ID" in text


def signing_detail(path: Path) -> str:
    result = subprocess.run(["/usr/bin/codesign", "-dv", "--verbose=2", str(path)], capture_output=True, text=True, timeout=60)
    if result.returncode:
        raise ValueError(f"cannot read the signature of {path.name}")
    return result.stdout + result.stderr


def team_of(detail: str) -> str | None:
    return next((line.strip()[len("TeamIdentifier="):] for line in detail.splitlines()
                 if line.strip().startswith("TeamIdentifier=")), None)


def identifier_of(detail: str) -> str | None:
    return next((line.strip()[len("Identifier="):] for line in detail.splitlines()
                 if line.strip().startswith("Identifier=")), None)


def spctl(*args: str | Path) -> str:
    # spctl reports its verdict on stderr; a rejection exits non-zero.
    result = subprocess.run(["/usr/sbin/spctl", *[str(a) for a in args]], capture_output=True, text=True, timeout=300)
    if result.returncode:
        raise ValueError("Gatekeeper rejected the release")
    return result.stdout + result.stderr


def verify_macos() -> None:
    """Everything a paid VocalCode checks before it swaps this bundle in
    (verify_dmg_before_mount, verify_bundle_identity and verify_bundle in its
    webui.rs), plus the free build's own linkage check."""
    dmg = ROOT / "dist-community/artifacts" / dmg_name(version())
    run("/usr/bin/codesign", "--verify", "--strict", "--verbose=2", dmg)
    if team_of(signing_detail(dmg)) != TEAM:
        raise ValueError("wrong DMG signing team")
    run("/usr/bin/xcrun", "stapler", "validate", dmg)
    if not gatekeeper_accepted(spctl("--assess", "--type", "open", "--context", "context:primary-signature", "--verbose=2", dmg)):
        raise ValueError("Gatekeeper did not accept the notarized disk image")
    with tempfile.TemporaryDirectory(prefix="vocalcode-verify-") as temp:
        mount = Path(temp) / "mount"
        mount.mkdir()
        try:
            run("/usr/bin/hdiutil", "attach", "-nobrowse", "-readonly", "-mountpoint", mount, dmg)
            # The paid updater takes exactly <volume>/VocalCode.app.
            app = mount / APP
            if app.is_symlink() or not app.is_dir():
                raise ValueError("the disk image has no VocalCode.app at its root")
            info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
            if (info.get("CFBundleIdentifier") != BUNDLE_ID or info.get("CFBundleShortVersionString") != version()
                    or info.get("CFBundleExecutable") != EXECUTABLE):
                raise ValueError("wrong signed app identity/version")
            run("/usr/bin/codesign", "--verify", "--deep", "--strict", "--verbose=2", app)
            detail = signing_detail(app)
            if team_of(detail) != TEAM or identifier_of(detail) != BUNDLE_ID:
                raise ValueError("wrong app signing team or identifier")
            requirement = subprocess.run(["/usr/bin/codesign", "-dr", "-", str(app)], capture_output=True, text=True, timeout=60)
            text = requirement.stdout + requirement.stderr
            if (requirement.returncode or f'identifier "{BUNDLE_ID}"' not in text
                    or f'certificate leaf[subject.OU] = "{TEAM}"' not in text):
                raise ValueError("the designated requirement does not identify VocalCode")
            run("/usr/bin/xcrun", "stapler", "validate", app)
            if not gatekeeper_accepted(spctl("-a", "-t", "exec", "-vv", app)):
                raise ValueError("Gatekeeper did not accept the notarized app")
            build = json.loads(run(app / "Contents/MacOS" / EXECUTABLE, "--build-info"))
            if build.get("edition") != "community" or build.get("version") != version():
                raise ValueError("signed app is not runnable or has the wrong edition")
            if not has_release_identity(build):
                raise ValueError("signed app is a development build")
        finally:
            run("/usr/bin/hdiutil", "detach", mount)
    print("DMG and app signature, notarization, identity and actual executable linkage verified "
          "as a paid VocalCode's updater verifies them.")


def check_notes() -> None:
    problems = release_notes_problems((ROOT / RELEASE_NOTES).read_text(encoding="utf-8"), version())
    if problems:
        raise SystemExit("RELEASE-NOTES.md is not the body for VocalCode " + version() + ":\n- " + "\n- ".join(problems))
    print(f"RELEASE-NOTES.md is the release body for VocalCode {version()}.")


def manifest() -> None:
    output = ROOT / "dist-community/artifacts"
    value = version()
    commit = run("git", "rev-parse", "HEAD").strip()
    # Schema and channel are the ones VocalCode's updater has read since the
    # first free release; only the asset names changed.
    data = {"schema": "vocalcode-community-update-v1", "channel": "community-stable", "source_commit": commit,
            "notes": "VocalCode is free and open source (AGPL-3.0). Local-first, with signed updates; no account or activation."}
    names = {"windows": WINDOWS_INSTALLER, "macos": dmg_name(value)}
    for platform, name in names.items():
        data[platform] = {"version": value, "url": f"https://github.com/{REPOSITORY}/releases/download/v{value}/{name}", **file_record(output / name)}
    (output / "latest.json").write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    source = output / source_archive_name(value)
    run("git", "archive", "--format=tar.gz", f"--prefix=VocalCode-{value}/", f"--output={source}", commit)
    lines = [f"{file_record(path)['sha256']}  {path.name}" for path in sorted(output.iterdir()) if path.is_file() and path.name != "SHA256SUMS"]
    (output / "SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="utf-8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("version", "check-notes", "build-macos", "sign-macos", "verify-macos", "manifest"))
    arguments = parser.parse_args()
    {"version": lambda: print(version()), "check-notes": check_notes, "build-macos": build_macos,
     "sign-macos": sign_macos, "verify-macos": verify_macos, "manifest": manifest}[arguments.command]()
