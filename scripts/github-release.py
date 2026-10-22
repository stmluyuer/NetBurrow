#!/usr/bin/env python3
"""Publish checked client packages or copy public releases without changing assets.

Writes only to GITHUB_REPOSITORY with its Actions GITHUB_TOKEN. Existing public
assets must match; only incomplete drafts may have their assets replaced.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import tomllib
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen
import zipfile


def api(path, method="GET", value=None, content_type="application/json"):
    data = json.dumps(value).encode() if isinstance(value, dict) else value
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "NetBurrow-release"}
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if data is not None:
        headers["Content-Type"] = content_type
    host = "https://uploads.github.com" if path.startswith("/upload/") else "https://api.github.com"
    if path.startswith("/upload/"):
        path = path.removeprefix("/upload")
    with urlopen(Request(host + path, data=data, headers=headers, method=method), timeout=120) as response:
        body = response.read()
        return json.loads(body) if body else None


def public_bytes(url):
    # Never send the repository token to public asset download redirects.
    with urlopen(Request(url, headers={"User-Agent": "NetBurrow-release"}), timeout=120) as response:
        return response.read()


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def validate_version(version):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError(f"Invalid release version: {version}")


def validate_assets(version, files):
    validate_version(version)
    expected = {"latest.json", f"NetBurrow-{version}-win-x64.zip"}
    if set(files) != expected:
        raise ValueError(f"Expected exactly {sorted(expected)}, received {sorted(files)}")
    metadata = files["latest.json"].read_bytes()
    manifest = json.loads(metadata.decode("utf-8-sig"))
    if (len(metadata) > 65536 or manifest["version"] != version
            or not manifest["notes"].strip() or len(manifest["notes"]) > 12000):
        raise ValueError("Invalid update manifest")
    with zipfile.ZipFile(files[f"NetBurrow-{version}-win-x64.zip"]) as archive:
        # Older packages also contained license/readme files; keep them intact.
        names = {Path(name.replace("\\", "/")).name for name in archive.namelist()}
        if not {"NetBurrow.exe", "netburrow-injector.exe", "netburrow_hook.dll"} <= names:
            raise ValueError("Client ZIP is missing a runtime component")
        damaged = archive.testzip()
        if damaged:
            raise ValueError(f"Corrupt ZIP entry: {damaged}")


def find_release(repo, tag):
    try:
        return api(f"/repos/{repo}/releases/tags/{tag}")
    except HTTPError as error:
        if error.code != 404:
            raise
    # Drafts may not be returned by the tag endpoint.
    for release in api(f"/repos/{repo}/releases?per_page=100"):
        if release["tag_name"] == tag:
            return release
    return None


def publish(repo, version, title, notes, files, latest, dry_run):
    validate_assets(version, files)
    for name, path in files.items():
        print(f"{repo} v{version}: {name} {digest(path.read_bytes())}", flush=True)
    if dry_run:
        return
    if not os.environ.get("GITHUB_TOKEN"):
        raise ValueError("GITHUB_TOKEN is required for publishing")
    tag = f"v{version}"
    release = find_release(repo, tag)
    if release is None:
        release = api(f"/repos/{repo}/releases", "POST", {
            "tag_name": tag, "target_commitish": os.environ["GITHUB_SHA"],
            "name": title, "body": notes, "draft": True, "prerelease": False,
        })
    if release["prerelease"]:
        raise ValueError(f"Refusing to replace a prerelease: {tag}")
    existing = {asset["name"]: asset for asset in release["assets"]}
    for name, path in files.items():
        data = path.read_bytes()
        asset = existing.get(name)
        if asset:
            actual = asset.get("digest") or digest(public_bytes(asset["browser_download_url"]))
            if actual == digest(data) and asset["size"] == len(data):
                continue
            if not release["draft"]:
                raise ValueError(f"Published asset differs; use a new version: {tag}/{name}")
            api(f"/repos/{repo}/releases/assets/{asset['id']}", "DELETE")
        elif not release["draft"]:
            raise ValueError(f"Published release is incomplete: {tag}/{name}")
        uploaded = api(
            f"/upload/repos/{repo}/releases/{release['id']}/assets?name={quote(name)}",
            "POST", data, "application/octet-stream",
        )
        if uploaded["size"] != len(data) or (uploaded.get("digest") and uploaded["digest"] != digest(data)):
            raise ValueError(f"Uploaded asset checksum mismatch: {name}")
    if release["draft"]:
        api(f"/repos/{repo}/releases/{release['id']}", "PATCH", {
            "draft": False, "name": title, "body": notes,
            "make_latest": "true" if latest else "false",
        })
    print(f"Verified https://github.com/{repo}/releases/tag/{tag}", flush=True)


def current_notes():
    version = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    validate_version(version)
    changelog = Path("CHANGELOG.md").read_text(encoding="utf-8")
    section = re.search(rf"^## {re.escape(version)}[：:][^\n]*\n(.*?)(?=^## |\Z)", changelog, re.M | re.S)
    if not section or not section[1].strip():
        raise ValueError(f"Missing changelog section for {version}")
    return version, section[1].strip() + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["notes", "publish", "copy"])
    parser.add_argument("--source")
    parser.add_argument("--tags", nargs="+")
    parser.add_argument("--historical", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    if args.command == "notes":
        version, notes = current_notes()
        Path(".local").mkdir(exist_ok=True)
        Path(".local/release-notes.txt").write_text(notes, encoding="utf-8")
        print(f"Prepared release notes for {version}")
        return
    repo = os.environ["GITHUB_REPOSITORY"]
    if args.command == "publish":
        version, notes = current_notes()
        archive = Path(f".local/dist/NetBurrow-{version}-win-x64.zip")
        with zipfile.ZipFile(archive) as package:
            if set(package.namelist()) != {"NetBurrow.exe", "netburrow-injector.exe", "netburrow_hook.dll"}:
                raise ValueError("New client package must contain only the three runtime files")
        commit = os.environ["GITHUB_SHA"]
        notes += (f"\n## 下载与源码\n\nWindows x64：`{archive.name}`，请完整解压。\n\n"
                  f"SHA-256: `{digest(archive.read_bytes()).split(':')[1]}`\n\n"
                  f"构建源码：[{commit}](https://github.com/{repo}/commit/{commit})。\n\n"
                  "GitHub 自动生成的 Source code 压缩包是源码，不是客户端。\n")
        files = {archive.name: archive, "latest.json": Path(f".local/dist/updates/{version}/latest.json")}
        publish(repo, version, f"NetBurrow v{version}", notes, files, True, args.dry_run)
        return
    if not args.source or not args.tags or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.source):
        parser.error("copy requires --source OWNER/REPO and --tags vX.Y.Z ...")
    for tag in args.tags:
        version = tag.removeprefix("v")
        validate_version(version)
        release = api(f"/repos/{args.source}/releases/tags/{tag}")
        if release["draft"] or release["prerelease"]:
            raise ValueError(f"Not a stable public release: {tag}")
        with tempfile.TemporaryDirectory() as folder:
            files = {}
            for asset in release["assets"]:
                name = asset["name"]
                if name not in {"latest.json", f"NetBurrow-{version}-win-x64.zip"}:
                    raise ValueError(f"Unexpected source asset: {name}")
                data = public_bytes(asset["browser_download_url"])
                if len(data) != asset["size"] or (asset.get("digest") and digest(data) != asset["digest"]):
                    raise ValueError(f"Source asset checksum mismatch: {name}")
                path = Path(folder, name)
                path.write_bytes(data)
                files[name] = path
            if args.historical:
                prefix = (f"> 历史发行迁移：原始发布时间 {release['published_at']}，"
                          f"[原发行页]({release['html_url']})。附件保持原样。"
                          "本标签仅用于保留下载入口，不代表原包的准确构建源码；"
                          "旧说明中的发行仓库源码 ZIP 提示适用于原发行页。\n\n")
            else:
                prefix = (f"> 旧客户端升级过渡包，与[主仓库发行版]({release['html_url']})附件完全相同。"
                          "升级到本版后，将从主仓库检查后续更新。\n\n")
            publish(repo, version, release["name"], prefix + (release["body"] or ""),
                    files, not args.historical, args.dry_run)


if __name__ == "__main__":
    main()
