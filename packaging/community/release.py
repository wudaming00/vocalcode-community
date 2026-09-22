"""Community packaging/release helpers. No commerce or production-site access."""
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
APP = "VocalCode Community.app"
BUNDLE_ID = "app.vocalcode.Community"
RUNTIMES = ("libonnxruntime.dylib", "libsherpa-onnx-c-api.dylib", "libsherpa-onnx-cxx-api.dylib")


def version() -> str:
    value = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", value):
        raise ValueError("community stable versions must be canonical X.Y.Z")
    return value


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


def build_macos() -> None:
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "release"
    volume = ROOT / "dist-community" / "macos"
    contents = volume / APP / "Contents"
    contents.mkdir(parents=True, exist_ok=False)
    for name in ("MacOS", "Frameworks", "Resources"):
        (contents / name).mkdir()
    shutil.copy2(target / "vocalcode-app", contents / "MacOS" / "VocalCode")
    (contents / "MacOS" / "VocalCode").chmod(0o755)
    for name in RUNTIMES:
        shutil.copy2(target / name, contents / "Frameworks" / name)
        if run("/usr/bin/lipo", "-archs", contents / "Frameworks" / name).strip() != "arm64":
            raise ValueError("unexpected native runtime architecture")
    data = plistlib.loads((ROOT / "packaging/macos/Info.plist").read_bytes())
    data.update(CFBundleName="VocalCode Community", CFBundleDisplayName="VocalCode Community",
                CFBundleIdentifier=BUNDLE_ID, CFBundleShortVersionString=version(), CFBundleVersion=version())
    # The provider uses the process-local service registration. Keep its port
    # spelling while giving users an unambiguous edition-specific menu item.
    for service in data.get("NSServices", []):
        service["NSMenuItem"]["default"] = "Add to VocalCode Community dictionary"
    (contents / "Info.plist").write_bytes(plistlib.dumps(data))
    run("python3", ROOT / "packaging/macos/make_icon.py", contents / "Resources/VocalCode.icns", timeout=300)
    copy_notices(contents / "Resources")
    (volume / "Applications").symlink_to("/Applications", target_is_directory=True)
    info = json.loads(run(contents / "MacOS/VocalCode", "--build-info"))
    if info.get("edition") != "community" or info.get("version") != version():
        raise ValueError("bundle is not the expected runnable community binary")
    run("/usr/bin/ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", volume, ROOT / "dist-community/macos-unsigned.zip")


def notarize(path: Path, key: Path, key_id: str, issuer: str) -> None:
    result = json.loads(run("/usr/bin/xcrun", "notarytool", "submit", path, "--key", key,
                            "--key-id", key_id, "--issuer", issuer, "--wait", "--timeout", "30m",
                            "--output-format", "json", timeout=1900))
    if result.get("status") != "Accepted":
        raise RuntimeError("Apple did not accept the notarization submission")


def sign_macos() -> None:
    temp_root = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    work = Path(tempfile.mkdtemp(prefix="vocalcode-community-signing-", dir=temp_root))
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
        dmg = output / f"VocalCodeCommunity-{version()}.dmg"
        if dmg.exists():
            raise ValueError("refusing to overwrite a release disk image")
        run("/usr/bin/hdiutil", "create", "-volname", "VocalCode Community", "-srcfolder", volume,
            "-format", "UDZO", dmg, timeout=300)
        run(*sign, dmg)
        notarize(dmg, notary_key, key_id, issuer)
        run("/usr/bin/xcrun", "stapler", "staple", dmg)
        run("/usr/bin/xcrun", "stapler", "validate", dmg)
        print("Signed and notarized community app and disk image; no installer executed with credentials.")
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


def verify_macos() -> None:
    dmg = ROOT / "dist-community/artifacts" / f"VocalCodeCommunity-{version()}.dmg"
    run("/usr/bin/codesign", "--verify", "--strict", dmg)
    detail = subprocess.run(["/usr/bin/codesign", "-dv", "--verbose=4", str(dmg)], capture_output=True, text=True, check=True)
    if f"TeamIdentifier={TEAM}" not in detail.stderr:
        raise ValueError("wrong DMG signing team")
    run("/usr/bin/xcrun", "stapler", "validate", dmg)
    run("/usr/sbin/spctl", "--assess", "--type", "open", "--context", "context:primary-signature", "--verbose=2", dmg)
    with tempfile.TemporaryDirectory(prefix="vocalcode-community-verify-") as temp:
        mount = Path(temp) / "mount"
        mount.mkdir()
        try:
            run("/usr/bin/hdiutil", "attach", "-nobrowse", "-readonly", "-mountpoint", mount, dmg)
            app = mount / APP
            info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
            if info.get("CFBundleIdentifier") != BUNDLE_ID or info.get("CFBundleShortVersionString") != version():
                raise ValueError("wrong signed community app identity/version")
            run("/usr/bin/codesign", "--verify", "--deep", "--strict", app)
            run("/usr/bin/xcrun", "stapler", "validate", app)
            run("/usr/sbin/spctl", "--assess", "--type", "execute", "--verbose=2", app)
            build = json.loads(run(app / "Contents/MacOS/VocalCode", "--build-info"))
            if build.get("edition") != "community" or build.get("version") != version():
                raise ValueError("signed app is not runnable or has the wrong edition")
        finally:
            run("/usr/bin/hdiutil", "detach", mount)
    print("DMG and app signature, notarization, identity and actual executable linkage verified.")


def manifest() -> None:
    output = ROOT / "dist-community/artifacts"
    value = version()
    commit = run("git", "rev-parse", "HEAD").strip()
    data = {"schema": "vocalcode-community-update-v1", "channel": "community-stable", "source_commit": commit,
            "notes": "Free, local-first community edition. Separate data and signed updates; no activation required."}
    names = {"windows": "VocalCodeCommunitySetup.exe", "macos": f"VocalCodeCommunity-{value}.dmg"}
    for platform, name in names.items():
        data[platform] = {"version": value, "url": f"https://github.com/{REPOSITORY}/releases/download/v{value}/{name}", **file_record(output / name)}
    (output / "latest.json").write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    source = output / f"VocalCodeCommunity-source-{value}.tar.gz"
    run("git", "archive", "--format=tar.gz", f"--prefix=VocalCodeCommunity-{value}/", f"--output={source}", commit)
    lines = [f"{file_record(path)['sha256']}  {path.name}" for path in sorted(output.iterdir()) if path.is_file() and path.name != "SHA256SUMS"]
    (output / "SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="utf-8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("version", "build-macos", "sign-macos", "verify-macos", "manifest"))
    arguments = parser.parse_args()
    {"version": lambda: print(version()), "build-macos": build_macos, "sign-macos": sign_macos,
     "verify-macos": verify_macos, "manifest": manifest}[arguments.command]()
