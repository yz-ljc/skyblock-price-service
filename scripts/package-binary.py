"""Package a static Linux release from any build host, preserving Unix modes."""

import argparse
import hashlib
import io
from pathlib import Path
import struct
import subprocess
import tarfile
from datetime import datetime, timezone


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", choices=("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    arch, machine = ("amd64", 62) if args.target.startswith("x86_64") else ("arm64", 183)
    binary = (root / "target" / args.target / "release/skyblock-price-service").read_bytes()
    if len(binary) < 64 or binary[:6] != b"\x7fELF\x02\x01":
        raise ValueError("Expected a 64-bit little-endian ELF executable")
    if struct.unpack_from("<H", binary, 18)[0] != machine:
        raise ValueError("Binary CPU architecture does not match the package")
    offset = struct.unpack_from("<Q", binary, 32)[0]
    size, count = struct.unpack_from("<HH", binary, 54)
    if size < 56 or offset + size * count > len(binary):
        raise ValueError("Invalid ELF program headers")
    for index in range(count):
        if struct.unpack_from("<I", binary, offset + index * size)[0] in (2, 3):
            raise ValueError("Expected a static executable without dynamic loader or dependencies")

    files = {"skyblock-price-service": (binary, 0o755)}
    for name in ("deploy/install.sh", "scripts/diagnose-upstream.sh", "config.example.toml", "THIRD_PARTY_NOTICES.txt", "docs/deployment.md", "docs/api.md"):
        # Git may check out CRLF on Windows; shell scripts and bundle text need LF.
        files[name] = ((root / name).read_bytes().replace(b"\r\n", b"\n"), 0o755 if name.endswith(".sh") else 0o644)
    version = subprocess.check_output(["rustc", "--version"], text=True).strip()
    now = datetime.now(timezone.utc)
    info = f"target={args.target}\nbuilt_at={now.isoformat()}\n{version}\n"
    files["BUILD_INFO.txt"] = (info.encode(), 0o644)
    sums = "".join(f"{hashlib.sha256(data).hexdigest()}  ./{name}\n" for name, (data, _) in sorted(files.items()))
    files["SHA256SUMS"] = (sums.encode(), 0o644)
    destination = root / "dist"
    destination.mkdir(exist_ok=True)
    archive = destination / f"skyblock-price-service-linux-{arch}.tar.gz"
    temporary = archive.with_suffix(".tmp")
    with tarfile.open(temporary, "w:gz", format=tarfile.USTAR_FORMAT) as bundle:
        for name, (data, mode) in sorted(files.items()):
            entry = tarfile.TarInfo(f"skyblock-price-service/{name}")
            entry.size, entry.mode, entry.mtime = len(data), mode, int(now.timestamp())
            bundle.addfile(entry, io.BytesIO(data))
    temporary.replace(archive)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(".gz.sha256").write_text(f"{digest}  {archive.name}\n", encoding="utf-8", newline="\n")
    print(f"Linux package: {archive}")


if __name__ == "__main__":
    main()
