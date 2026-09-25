#!/usr/bin/env python3
"""Write package-manager manifests for a release from its built artifacts.

Usage: manifests.py <version> <artifacts dir> <out dir>

Writes a Homebrew cask (goro.rb), a Scoop manifest (goro.json) and winget manifests
(dev.goro.Goro.*.yaml), each pointing at the GitHub release assets with their SHA-256.
Fails if an expected artifact is missing.
"""
import hashlib
import json
import sys
from pathlib import Path

REPO = "kenryu42/goro"


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main() -> int:
    version, artifacts, out = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
    url = f"https://github.com/{REPO}/releases/download/v{version}"
    files = {
        "mac": f"Goro-{version}-macos-universal.zip",
        "win_x64": f"goro-{version}-windows-x86_64.zip",
        "win_arm64": f"goro-{version}-windows-aarch64.zip",
    }
    missing = [f for f in files.values() if not (artifacts / f).exists()]
    if missing:
        print(f"missing artifacts: {missing}", file=sys.stderr)
        return 1
    sums = {key: sha256(artifacts / name) for key, name in files.items()}
    out.mkdir(parents=True, exist_ok=True)

    (out / "goro.rb").write_text(
        f'''cask "goro" do
  version "{version}"
  sha256 "{sums["mac"]}"

  url "{url}/{files["mac"]}"
  name "Goro"
  desc "Instant, native review for agent-written code changes"
  homepage "https://github.com/{REPO}"

  depends_on macos: ">= :ventura"

  app "Goro.app"
  binary "#{{appdir}}/Goro.app/Contents/MacOS/goro"
end
'''
    )

    scoop = {
        "version": version,
        "description": "Instant, native review for agent-written code changes",
        "homepage": f"https://github.com/{REPO}",
        "license": "Apache-2.0 OR MIT",
        "architecture": {
            "64bit": {"url": f"{url}/{files['win_x64']}", "hash": sums["win_x64"]},
            "arm64": {"url": f"{url}/{files['win_arm64']}", "hash": sums["win_arm64"]},
        },
        "bin": "goro.exe",
        "shortcuts": [["goro.exe", "Goro"]],
    }
    (out / "goro.json").write_text(json.dumps(scoop, indent=2) + "\n")

    header = "# yaml-language-server: $schema=https://aka.ms/winget-manifest.{kind}.1.6.0.schema.json\n"
    common = f"PackageIdentifier: dev.goro.Goro\nPackageVersion: {version}\n"
    (out / "dev.goro.Goro.yaml").write_text(
        header.format(kind="version")
        + common
        + "DefaultLocale: en-US\nManifestType: version\nManifestVersion: 1.6.0\n"
    )
    installers = "".join(
        f"""- Architecture: {arch}
  InstallerType: zip
  NestedInstallerType: portable
  NestedInstallerFiles:
  - RelativeFilePath: goro.exe
    PortableCommandAlias: goro
  InstallerUrl: {url}/{files[key]}
  InstallerSha256: {sums[key].upper()}
"""
        for arch, key in [("x64", "win_x64"), ("arm64", "win_arm64")]
    )
    (out / "dev.goro.Goro.installer.yaml").write_text(
        header.format(kind="installer")
        + common
        + "Installers:\n"
        + installers
        + "ManifestType: installer\nManifestVersion: 1.6.0\n"
    )
    (out / "dev.goro.Goro.locale.en-US.yaml").write_text(
        header.format(kind="defaultLocale")
        + common
        + f"""PackageLocale: en-US
Publisher: Goro
PackageName: Goro
License: Apache-2.0 OR MIT
ShortDescription: Instant, native review for agent-written code changes
PackageUrl: https://github.com/{REPO}
ManifestType: defaultLocale
ManifestVersion: 1.6.0
"""
    )
    print(f"manifests written to {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
