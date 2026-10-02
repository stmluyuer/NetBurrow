#!/usr/bin/env python3
"""Collect notices for every registry package in Cargo.lock (Python 3.11+).

Uses local Cargo sources. Missing crate-level licenses are read from the upstream
repository at the commit recorded in .cargo_vcs_info.json, never from HEAD.
Run from any directory; --check compares without changing the checked-in report.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import struct
import sys
import tomllib
import urllib.request
import urllib.error
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
NOTICE_NAME = re.compile(r"^(?:licen[cs]e|copying|copyright|notices?|authors|ofl|ufl)(?:[._-]|$)", re.I)
FETCHED = {}
KNOWN_DISPATCH_GAP = "dispatch 0.2.0: upstream license retrieval failed: no root license files found at recorded upstream commit"
UPSTREAM_FILES = {
    "AccessKit/accesskit": ("AUTHORS", "LICENSE-APACHE", "LICENSE-MIT", "LICENSE.chromium"),
    "DoumanAsh/clipboard-win": ("LICENSE",),
    "SSheldon/rust-dispatch": (),
    "brendanzab/gl-rs": ("LICENSE",),
    "emilk/egui": ("LICENSE-APACHE", "LICENSE-MIT"),
    "jni-rs/jni-rs": ("LICENSE-APACHE", "LICENSE-MIT"),
    "jni-rs/jni-sys": ("LICENSE-APACHE", "LICENSE-MIT"),
    "rust-mobile/ndk": ("LICENSE-APACHE", "LICENSE-MIT"),
    "rust-windowing/android-ndk-rs": ("LICENSE-APACHE", "LICENSE-MIT"),
    "madsmtm/objc2": ("LICENSE.txt", "LICENSE.md"),
    "aclysma/profiling": ("LICENSE-APACHE", "LICENSE-MIT"),
}


def fetch(url):
    if url not in FETCHED:
        request = urllib.request.Request(url, headers={"User-Agent": "NetBurrow-license-inventory"})
        with urllib.request.urlopen(request, timeout=30) as response:
            FETCHED[url] = response.read().decode("utf-8-sig")
    return FETCHED[url]


def notice_file(path):
    return bool(NOTICE_NAME.match(path.name)) and path.suffix.lower() not in {".rs", ".toml", ".json"}


def font_notices(path):
    """Read copyright/trademark/license name records from shipped TrueType fonts."""
    data = path.read_bytes()
    count = struct.unpack_from(">H", data, 4)[0]
    for index in range(count):
        tag, _, offset, _ = struct.unpack_from(">4sIII", data, 12 + index * 16)
        if tag != b"name":
            continue
        _, records, strings = struct.unpack_from(">HHH", data, offset)
        result = []
        for record in range(records):
            platform, encoding, language, name, length, start = struct.unpack_from(">HHHHHH", data, offset + 6 + record * 12)
            if name not in {0, 7, 13, 14}:
                continue
            raw = data[offset + strings + start:offset + strings + start + length]
            text = raw.decode("utf-16-be" if platform in {0, 3} else "mac_roman")
            entry = f"Name ID {name}:\n{text}"
            if entry not in result:
                result.append(entry)
        return "\n\n".join(result)
    raise ValueError(f"No TrueType name table in {path.name}")


def upstream_notices(manifest, directory):
    vcs = json.loads((directory / ".cargo_vcs_info.json").read_text(encoding="utf-8"))
    sha = vcs["git"]["sha1"]
    repository = manifest.get("repository", "")
    match = re.match(r"https?://github.com/([^/]+/[^/]+)", repository)
    if not match or not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("no supported immutable upstream source")
    slug = match[1].removesuffix(".git")
    if slug in UPSTREAM_FILES:
        # These paths were identified in the official fixed-commit root trees.
        # Raw URLs avoid GitHub API rate limits during repeatable regeneration.
        def read_notice(name):
            url = f"https://raw.githubusercontent.com/{slug}/{sha}/{name}"
            try:
                return url, fetch(url)
            except urllib.error.HTTPError as error:
                if error.code == 404:
                    return None
                raise
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            notices = [notice for notice in pool.map(read_notice, UPSTREAM_FILES[slug]) if notice]
        if not notices:
            raise ValueError("no root license files found at recorded upstream commit")
        return notices
    tree_url = f"https://api.github.com/repos/{slug}/git/trees/{sha}"
    tree = json.loads(fetch(tree_url))["tree"]
    notices = []
    for entry in tree:
        if entry["type"] == "blob" and notice_file(Path(entry["path"])):
            url = f"https://raw.githubusercontent.com/{slug}/{sha}/{entry['path']}"
            notices.append((url, fetch(url)))
        elif entry["type"] == "tree" and entry["path"].lower() in {"license", "licenses"}:
            children = json.loads(fetch(entry["url"]))["tree"]
            for child in children:
                if child["type"] == "blob":
                    url = f"https://raw.githubusercontent.com/{slug}/{sha}/{entry['path']}/{child['path']}"
                    notices.append((url, fetch(url)))
    if not notices:
        raise ValueError("no root license files found at recorded upstream commit")
    return notices


def generate(registry):
    lock = (ROOT / "Cargo.lock").read_bytes()
    packages = sorted(
        (p for p in tomllib.loads(lock.decode("utf-8"))["package"] if p.get("source")),
        key=lambda p: (p["name"], p["version"]),
    )
    documents = {}
    entries = []
    issues = []
    upstream_packages = 0
    embedded_notices = 0
    for package in packages:
        label = f"{package['name']} {package['version']}"
        candidates = list(registry.glob(f"*/{package['name']}-{package['version']}"))
        if len(candidates) != 1:
            issues.append(f"{label}: expected one local source directory, found {len(candidates)}")
            entries.append(f"PACKAGE: {label}\nSource: {package['source']}\nUNRESOLVED: local source unavailable")
            continue
        directory = candidates[0]
        manifest = tomllib.loads((directory / "Cargo.toml").read_text(encoding="utf-8"))["package"]
        notice_paths = sorted(p for p in directory.rglob("*") if p.is_file() and notice_file(p))
        explicit = manifest.get("license-file")
        if explicit and (directory / explicit).is_file():
            notice_paths = sorted(set(notice_paths + [directory / explicit]))
        # Hack's font license has a font-specific filename; include every shipped
        # font text notice rather than relying on LICENSE/OFL naming alone.
        if package["name"] == "epaint_default_fonts":
            notice_paths = sorted(set(notice_paths + list((directory / "fonts").glob("*.txt"))))
        notices = [(f"crate:{label}/{p.relative_to(directory).as_posix()}", p.read_text(encoding="utf-8-sig")) for p in notice_paths]
        if package["name"] == "epaint_default_fonts":
            for path in sorted((directory / "fonts").glob("*.ttf")):
                embedded = font_notices(path)
                if embedded:
                    notices.append((f"crate:{label}/{path.relative_to(directory).as_posix()} (embedded name records)", embedded))
        root_license = any(p.parent == directory and "license" in p.name.lower() for p in notice_paths)
        missing_text = not notices
        if missing_text or package["name"] == "epaint_default_fonts" and not root_license:
            try:
                notices.extend(upstream_notices(manifest, directory))
                upstream_packages += 1
            except (OSError, ValueError, KeyError) as error:
                issues.append(f"{label}: upstream license retrieval failed: {error}")
        # Khronos XML registries carry their own copyright and grant, independent
        # of the Apache license declared for the Rust crate wrapper.
        if package["name"] == "khronos_api":
            for path in sorted(directory.rglob("*.xml")):
                source = path.read_text(encoding="utf-8")
                for index, block in enumerate(re.findall(r"<!--.*?-->|<comment>.*?</comment>", source, re.S)):
                    if re.search(r"copyright|permission is hereby granted|licensed under", block, re.I):
                        notices.append((f"crate:{label}/{path.relative_to(directory).as_posix()} (notice {index + 1})", block))
                        embedded_notices += 1
        lines = [f"PACKAGE: {label}", f"Source: {package['source']}", f"Cargo checksum: {package.get('checksum', 'not recorded')}", f"Declared license: {manifest.get('license', 'see license-file')}", f"Repository: {manifest.get('repository', 'not declared')}"]
        if not notices:
            lines.append("UNRESOLVED: no upstream license text or copyright notice found; declaration alone is not a full notice.")
            if package["name"] == "dispatch" and package["version"] == "0.2.0":
                lines.extend([
                    "Known scope exception: Apple Grand Central Dispatch wrapper, retained in this",
                    "conservative lockfile inventory for non-release Apple targets. It is absent",
                    "from the Windows x86_64 application, Windows i686 Hook/injector, and Linux",
                    "x86_64 Relay dependency graphs checked for this release. The crate and its",
                    "recorded upstream commit supply only an MIT declaration, not license text.",
                    "No copyright holder or license grant has been fabricated for this package.",
                    "Recheck the target dependency graphs if release targets or dependencies change.",
                ])
        for source, body in notices:
            body = body.replace("\r\n", "\n").rstrip() + "\n"
            if not body.strip():
                issues.append(f"{label}: empty notice {source}")
                continue
            digest = hashlib.sha256(body.encode("utf-8")).hexdigest()
            documents.setdefault(digest, body)
            lines.append(f"Notice SHA256: {digest}\n  Origin: {source}")
        if package["name"] == "epaint_default_fonts":
            lines.extend([
                "Embedded font mapping (from src/lib.rs and fonts/):",
                "  Hack-Regular.ttf -> fonts/Hack-Regular.txt (MIT and Bitstream Vera terms)",
                "  NotoEmoji-Regular.ttf -> fonts/OFL.txt (SIL Open Font License 1.1)",
                "  Ubuntu-Light.ttf -> fonts/UFL.txt (Ubuntu Font Licence 1.0)",
                "  emoji-icon-font.ttf -> fonts/emoji-icon-font-mit-license.txt (MIT)",
            ])
        entries.append("\n".join(lines))
    header = [
        "NETBURROW THIRD-PARTY LICENSES AND NOTICES",
        "",
        "Generated by: python scripts/generate-third-party-licenses.py",
        f"Cargo.lock SHA256: {hashlib.sha256(lock).hexdigest()}",
        f"Inventory: {len(packages)} locked third-party packages; {len(documents)} distinct notice texts.",
        f"Upstream supplement: {upstream_packages} packages, using recorded source commits.",
        f"Embedded Khronos XML notices: {embedded_notices} occurrences (identical texts deduplicated).",
        "",
        "SCOPE",
        "This is a conservative Cargo.lock inventory, including build, optional, and",
        "non-Windows dependencies. Listing a package does not mean every distributed",
        "binary links it. NetBurrow workspace crates are covered by LICENSE separately.",
        "Notices are copied from downloaded crate sources and, where omitted by crate",
        "packaging, official upstream repositories at .cargo_vcs_info.json commits.",
        "Embedded default font notices and Khronos registry notices are included.",
        "SPDX expressions below are upstream declarations, not replacement license text.",
        "Alternative licenses remain alternatives; copying all supplied texts does not",
        "elect every alternative. In particular, r-efi offers MIT/Apache-2.0 as",
        "alternatives to LGPL, and r-efi or other optional packages need not be linked.",
        "Font licenses and Unicode data licenses are separate from Rust code licenses.",
        "Known non-release exception: dispatch 0.2.0 (Apple GCD) has an MIT declaration",
        "but no license text in its package or recorded upstream commit; see its entry.",
        "This document does not grant rights to Steam, game assets, user-selected fonts,",
        "or other software installed separately, and is not an exhaustive source-code",
        "copyright audit. Existing file-level notices in third-party source still apply.",
        "The immutable notice bodies are deduplicated by SHA256; each package entry",
        "identifies every collected notice and its source before the full-text appendix.",
        "",
        "COLLECTION ISSUES",
        *(issues or ["None detected within the stated collection scope."]),
        "",
        "PACKAGE INVENTORY",
        "=" * 78,
    ]
    appendix = ["FULL NOTICE TEXTS", "=" * 78]
    for digest, body in documents.items():
        appendix.extend([f"BEGIN NOTICE SHA256: {digest}", body.rstrip(), f"END NOTICE SHA256: {digest}", ""])
    result = "\n".join(header) + "\n\n" + "\n\n".join(entries) + "\n\n" + "\n".join(appendix)
    return result, issues, len(packages), len(documents)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    cargo_home = os.environ.get("CARGO_HOME")
    if cargo_home:
        registry = Path(cargo_home) / "registry/src"
    else:
        registry = Path.home() / ".cargo/registry/src"
        if not registry.is_dir():
            registry = ROOT / ".local/cargo/registry/src"
    parser.add_argument("--registry", type=Path, default=registry, help="Cargo registry/src directory (defaults to CARGO_HOME, user Cargo cache, then existing repository cache)")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    report, issues, packages, notices = generate(args.registry)
    target = ROOT / "THIRD_PARTY_LICENSES.txt"
    unexpected = [issue for issue in issues if issue != KNOWN_DISPATCH_GAP]
    if unexpected:
        print("Collection failed; existing THIRD_PARTY_LICENSES.txt was preserved.", file=sys.stderr)
        for issue in issues:
            print(issue, file=sys.stderr)
        return 1
    if args.check:
        if not target.exists() or target.read_text(encoding="utf-8") != report:
            print("THIRD_PARTY_LICENSES.txt is out of date", file=sys.stderr)
            return 1
    else:
        target.write_text(report, encoding="utf-8", newline="\n")
    print(f"{packages} packages; {notices} distinct notice texts; {len(issues)} collection issues")
    for issue in issues:
        print(issue, file=sys.stderr)
    return 1 if issues else 0


if __name__ == "__main__":
    sys.exit(main())
