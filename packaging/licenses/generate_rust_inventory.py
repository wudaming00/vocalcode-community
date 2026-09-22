#!/usr/bin/env python3
"""Generate/check the reviewed Rust dependency inventory and packaged notices."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LICENSE_ROOT = ROOT / "THIRD-PARTY-LICENSES"
INVENTORY = LICENSE_ROOT / "Rust-dependencies.json"
LICENSES = LICENSE_ROOT / "Rust-crate-license-files.txt"
LICENSE_NAME = re.compile(r"^(?:licen[cs]e|copying|notice)(?:$|[-._])", re.IGNORECASE)

# Some crates intentionally publish without the workspace-level license file.
# Every such crate/version must be reviewed here. A new or upgraded package with
# no packaged legal file therefore stops the release instead of quietly falling
# back to an SPDX label.
REVIEWED_MISSING_ARCHIVES = {
    # sonora-aec3 is a member of the Sonora workspace. Its 0.2.0 crate archive
    # declares BSD-3-Clause but omits the workspace-level LICENSE that is
    # included by the sibling `sonora` crate from the same tagged repository.
    ("sonora-aec3", "0.2.0"): ("BSD-3-Clause", ("Sonora-LICENSE.txt",)),
    ("libappindicator-sys", "0.9.0"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("cesu8", "1.1.0"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("dasp_sample", "0.11.0"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("defmt-parser", "1.0.0"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("dispatch2", "0.3.1"): ("Apache-2.0", ("Apache-2.0.txt", "Objc2-LICENSE.md")),
    ("evdev-rs", "0.4.0"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("evdev-sys", "0.2.6"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("jni", "0.22.4"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("jni-macros", "0.22.4"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("jni-sys-macros", "0.4.1"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("ndk", "0.9.0"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("ndk-context", "0.1.1"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("ndk-sys", "0.6.0+11769913"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("tao-macros", "0.1.3"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("winapi-i686-pc-windows-gnu", "0.4.0"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("winapi-x86_64-pc-windows-gnu", "0.4.0"): ("Apache-2.0", ("Apache-2.0.txt",)),
    ("selectors", "0.36.1"): ("MPL-2.0", ("MPL-2.0.txt",)),
    # The 0.5.5 meta-crate archive omits the workspace-level file. Its
    # manifest declares MPL-2.0 and the tagged upstream project applies MPL
    # 2.0 to all contributions and source files.
    ("symphonia", "0.5.5"): ("MPL-2.0", ("MPL-2.0.txt",)),
    ("block", "0.1.6"): ("MIT", ("MIT.txt",)),
    ("block2", "0.6.2"): ("MIT", ("MIT.txt", "Objc2-LICENSE.md")),
    ("dlopen2", "0.8.2"): ("MIT", ("Dlopen2-LICENSE.txt",)),
    ("dlopen2_derive", "0.4.3"): ("MIT", ("Dlopen2-LICENSE.txt",)),
    ("malloc_buf", "0.0.6"): ("MIT", ("MIT.txt",)),
    ("objc2", "0.6.4"): ("MIT", ("MIT.txt", "Objc2-LICENSE.md")),
    ("objc2-encode", "4.1.0"): ("MIT", ("MIT.txt", "Objc2-LICENSE.md")),
    ("objc2-foundation", "0.3.2"): ("MIT", ("MIT.txt", "Objc2-LICENSE.md")),
    ("webview2-com", "0.38.2"): ("MIT", ("WebView2Rs-LICENSE.txt",)),
    ("webview2-com-macros", "0.8.1"): ("MIT", ("WebView2Rs-LICENSE.txt",)),
    ("webview2-com-sys", "0.38.2"): ("MIT", ("WebView2Rs-LICENSE.txt",)),
}

# Symphonia 0.5.5 publishes each workspace crate without the repository's
# top-level MPL file. Every active crate below declares MPL-2.0 and comes from
# the same tagged upstream repository, so the packaged canonical MPL text is
# the reviewed legal reference for each archive.
for _symphonia_crate in (
    "symphonia-bundle-flac",
    "symphonia-bundle-mp3",
    "symphonia-codec-aac",
    "symphonia-codec-alac",
    "symphonia-codec-pcm",
    "symphonia-codec-vorbis",
    "symphonia-core",
    "symphonia-format-caf",
    "symphonia-format-isomp4",
    "symphonia-format-mkv",
    "symphonia-format-ogg",
    "symphonia-format-riff",
    "symphonia-metadata",
    "symphonia-utils-xiph",
):
    REVIEWED_MISSING_ARCHIVES[(_symphonia_crate, "0.5.5")] = (
        "MPL-2.0",
        ("MPL-2.0.txt",),
    )

for _objc2_framework in (
    "objc2-app-kit",
    "objc2-audio-toolbox",
    "objc2-av-foundation",
    "objc2-avf-audio",
    "objc2-cloud-kit",
    "objc2-core-audio",
    "objc2-core-audio-types",
    "objc2-core-data",
    "objc2-core-foundation",
    "objc2-core-graphics",
    "objc2-core-image",
    "objc2-core-location",
    "objc2-core-text",
    "objc2-io-surface",
    "objc2-quartz-core",
    "objc2-ui-kit",
    "objc2-user-notifications",
    "objc2-web-kit",
):
    REVIEWED_MISSING_ARCHIVES[(_objc2_framework, "0.3.2")] = (
        "Apache-2.0",
        ("Apache-2.0.txt", "Objc2-LICENSE.md"),
    )
REVIEWED_MISSING_ARCHIVES[("objc2-exception-helper", "0.1.1")] = (
    "Apache-2.0",
    ("Apache-2.0.txt", "Objc2-LICENSE.md"),
)


def cargo_executable() -> str:
    selected = os.environ.get("VOCALCODE_RELEASE_CARGO")
    if selected is None:
        if os.environ.get("GITHUB_ACTIONS") == "true":
            raise RuntimeError("GitHub release inventory requires the reviewed Cargo path")
        return "cargo"
    if not selected or "\n" in selected or "\r" in selected:
        raise RuntimeError("reviewed Cargo path is empty or malformed")
    path = Path(selected)
    if not path.is_absolute() or not path.is_file():
        raise RuntimeError("reviewed Cargo path must be an absolute executable file")
    if os.environ.get("GITHUB_ACTIONS") == "true":
        root_value = os.environ.get("VOCALCODE_RELEASE_JOB_ROOT", "")
        root = Path(root_value)
        if (
            not root_value
            or not root.is_absolute()
            or not root.is_dir()
            or root.is_symlink()
            or root.resolve() != root
        ):
            raise RuntimeError("GitHub release job root is not a physical directory")
        expected = root / "cargo" / "bin" / path.name
        if path != expected:
            raise RuntimeError("reviewed Cargo path is outside this release job")
        if path.resolve().parent != expected.parent.resolve():
            raise RuntimeError("reviewed Cargo proxy resolves outside its private bin directory")
    elif path.is_symlink():
        raise RuntimeError("local reviewed Cargo path must not be a symlink")
    return str(path)


def metadata() -> dict:
    result = subprocess.run(
        [cargo_executable(), "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    return json.loads(result.stdout)


def normalized_text(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="replace").replace("\r\n", "\n").rstrip() + "\n"


def build() -> tuple[str, str]:
    graph = metadata()
    workspace = set(graph["workspace_members"])
    nodes = {node["id"]: set(node.get("features", [])) for node in graph["resolve"]["nodes"]}
    packages = []
    license_groups: dict[str, dict] = {}
    packages_without_files = []
    seen_missing_reviews = set()

    # The two reviewed sherpa wrappers are vendored workspace members so their
    # hardened native build can run in the normal workspace gates. They remain
    # third-party Apache-2.0 projects and must still appear in the shipped Rust
    # inventory; other workspace members are VocalCode's own code.
    vendored_third_party = {"sherpa-onnx", "sherpa-onnx-sys"}
    for package in sorted(
        (
            item
            for item in graph["packages"]
            if item["id"] not in workspace or item["name"] in vendored_third_party
        ),
        key=lambda item: (item["name"].lower(), item["version"], item["id"]),
    ):
        license_expression = package.get("license")
        if not license_expression:
            raise SystemExit(f"dependency has no declared license: {package['name']} {package['version']}")
        entry = {
            "name": package["name"],
            "version": package["version"],
            "license": license_expression,
            "source": package.get("source"),
            "repository": package.get("repository"),
            "authors": package.get("authors") or [],
        }
        if package["name"] == "zhconv" and package["version"] == "0.4.1":
            features = nodes.get(package["id"], set())
            forbidden = sorted(
                feature for feature in features
                if feature == "compress" or feature.startswith("mediawiki") or feature == "_mediawiki-base"
            )
            if forbidden or "opencc" not in features:
                raise SystemExit(
                    "zhconv exception is valid only for opencc without mediawiki/compress; "
                    f"active features: {sorted(features)}"
                )
            entry["reviewed_feature_exception"] = {
                "active_features": sorted(features),
                "code_license": "MIT OR Apache-2.0 (Apache-2.0 selected)",
                "data_license": "OpenCC Apache-2.0",
                "excluded": ["MediaWiki GPL-2.0-or-later tables", "compression dependencies"],
                "evidence": "https://github.com/Gowee/zhconv-rs/blob/v0.4.1/README.md#license",
            }
        packages.append(entry)

        root = Path(package["manifest_path"]).parent
        files = sorted(
            path for path in root.iterdir()
            if path.is_file() and LICENSE_NAME.match(path.name) and path.stat().st_size <= 1_000_000
        )
        if not files:
            key = (package["name"], package["version"])
            review = REVIEWED_MISSING_ARCHIVES.get(key)
            if review is None:
                raise SystemExit(
                    "dependency archive has no license/notice file and has not been reviewed: "
                    f"{package['name']} {package['version']} ({license_expression})"
                )
            selected_license, references = review
            if selected_license not in license_expression:
                raise SystemExit(
                    f"invalid reviewed license selection for {package['name']} {package['version']}: "
                    f"{selected_license} is not offered by {license_expression}"
                )
            missing_references = [name for name in references if not (LICENSE_ROOT / name).is_file()]
            if missing_references:
                raise SystemExit(
                    f"reviewed legal reference missing for {package['name']} {package['version']}: "
                    + ", ".join(missing_references)
                )
            attribution = ", ".join(package.get("authors") or [])
            if not attribution:
                attribution = package.get("repository") or (
                    f"https://crates.io/crates/{package['name']}/{package['version']}"
                )
            repository = package.get("repository") or (
                f"https://crates.io/crates/{package['name']}/{package['version']}"
            )
            packages_without_files.append(
                f"{package['name']} {package['version']}\n"
                f"  Declared SPDX expression: {license_expression}\n"
                f"  Reviewed selection: {selected_license}\n"
                f"  Attribution/project: {attribution}\n"
                f"  Source: {repository}\n"
                f"  Shipped legal text: {', '.join(references)}"
            )
            seen_missing_reviews.add(key)
        for path in files:
            content = normalized_text(path)
            digest = hashlib.sha256(content.encode("utf-8")).hexdigest()
            group = license_groups.setdefault(digest, {"text": content, "packages": []})
            group["packages"].append(f"{package['name']} {package['version']} ({path.name})")

    stale_reviews = sorted(set(REVIEWED_MISSING_ARCHIVES) - seen_missing_reviews)
    if stale_reviews:
        formatted = ", ".join(f"{name} {version}" for name, version in stale_reviews)
        raise SystemExit("stale missing-license review entries must be removed: " + formatted)

    lock_hash = hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest()
    inventory = {
        "schema": 1,
        "cargo_lock_sha256": lock_hash,
        "package_count": len(packages),
        "packages": packages,
    }
    inventory_text = json.dumps(inventory, ensure_ascii=False, indent=2, sort_keys=True) + "\n"

    sections = [
        "RUST DEPENDENCY LICENSE FILES\n",
        "Generated from the exact Cargo.lock dependency closure. Each unique license/notice file "
        "actually present in a crate archive is reproduced once and names every archive that supplied "
        "it. A small reviewed allowlist covers archives that omit a workspace-level legal file; each "
        "entry records the selected offered license, attribution/project evidence, source, and the "
        "separately shipped legal text. Any new, removed, or upgraded omission fails generation.\n\n",
        "REVIEWED CRATE ARCHIVES WITHOUT A PACKAGED LICENSE FILE\n\n",
        "\n\n".join(packages_without_files) + "\n",
    ]
    for digest, group in sorted(license_groups.items()):
        sections.extend([
            "\n" + "=" * 78 + "\n",
            f"SHA-256: {digest}\n",
            "Supplied by:\n  " + "\n  ".join(sorted(group["packages"])) + "\n",
            "-" * 78 + "\n",
            group["text"],
        ])
    return inventory_text, "".join(sections)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if args.write == args.check:
        raise SystemExit("choose exactly one of --write or --check")
    inventory, licenses = build()
    if args.write:
        LICENSE_ROOT.mkdir(parents=True, exist_ok=True)
        INVENTORY.write_text(inventory, encoding="utf-8", newline="\n")
        LICENSES.write_text(licenses, encoding="utf-8", newline="\n")
        print(f"wrote {INVENTORY.relative_to(ROOT)} and {LICENSES.relative_to(ROOT)}")
        return
    failures = []
    for path, wanted in ((INVENTORY, inventory), (LICENSES, licenses)):
        if not path.is_file() or path.read_text(encoding="utf-8") != wanted:
            failures.append(str(path.relative_to(ROOT)))
    if failures:
        raise SystemExit(
            "Cargo.lock/dependency notices changed; regenerate and review: " + ", ".join(failures)
        )
    print("Rust dependency inventory and packaged license files match Cargo.lock")


if __name__ == "__main__":
    main()
