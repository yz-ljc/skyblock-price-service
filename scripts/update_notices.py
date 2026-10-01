"""Rebuild dependency notices from checksum-verified Cargo archives; no network access."""

import hashlib
import os
from pathlib import Path
import tarfile
import tomllib


def main():
    root = Path(__file__).resolve().parent.parent
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    licenses = {}
    packages = []
    for package in lock["package"]:
        if not package.get("source", "").startswith("registry+"):
            continue
        name, version = package["name"], package["version"]
        filename = f"{name}-{version}.crate"
        archives = list((cargo_home / "registry" / "cache").glob(f"*/{filename}"))
        archive = next((p for p in archives if hashlib.sha256(p.read_bytes()).hexdigest() == package["checksum"]), None)
        if archive is None:
            raise RuntimeError(f"Missing checksum-verified archive: {filename}; run cargo fetch --locked")
        references = []
        with tarfile.open(archive, "r:gz") as source:
            manifest_file = source.extractfile(f"{name}-{version}/Cargo.toml")
            manifest = tomllib.loads(manifest_file.read().decode("utf-8"))
            declaration = manifest["package"].get("license", manifest["package"].get("license-file", "unspecified"))
            for member in source.getmembers():
                basename = Path(member.name).name.upper()
                if not member.isfile() or not basename.startswith(("LICENSE", "COPYING", "COPYRIGHT", "NOTICE", "AUTHORS")):
                    continue
                if member.size > 262144:
                    raise RuntimeError(f"Review oversized license file: {member.name}")
                text = source.extractfile(member).read().decode("utf-8", errors="strict").replace("\r\n", "\n")
                digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
                if digest not in licenses:
                    licenses[digest] = (len(licenses) + 1, text)
                references.append(f"{member.name.split('/', 1)[1]} -> [{licenses[digest][0]}]")
        if not references:
            # Some optional packages omit the referenced LICENSE from their published archive.
            # Retain the upstream declaration explicitly rather than inventing copyright text.
            references.append("No separate license text included in the published crate archive; see upstream source declaration.")
            print(f"Archive has a declaration only: {filename} ({declaration})")
        packages.append(f"{name} {version}\n  SPDX/license declaration: {declaration}\n"
                        f"  Source: https://crates.io/crates/{name}/{version}\n  " + "\n  ".join(references))
    header = ("Third-party notices for SkyBlock Price Service\n\n"
              "This file covers registry dependencies in Cargo.lock, including optional and platform-specific packages.\n"
              "Not every listed package is included in every binary. License alternatives retain their original declarations.\n"
              "License texts below are reproduced unchanged except for LF line endings; identical texts are shared.\n"
              "Generated from checksum-verified Cargo source archives using scripts/update_notices.py.\n\n")
    sections = [f"[{number}]\n{text}" for number, text in licenses.values()]
    (root / "THIRD_PARTY_NOTICES.txt").write_text(header + "\n\n".join(packages + sections) + "\n", encoding="utf-8", newline="\n")
    print(f"Recorded {len(packages)} packages and {len(licenses)} distinct notice texts.")


if __name__ == "__main__":
    main()
