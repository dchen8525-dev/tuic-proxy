#!/usr/bin/env python3
"""Build a Debian/Ubuntu .deb for psk-quic-proxy using only the standard library.

The deb format is a plain ar archive with three members:

    debian-binary    the single line "2.0\\n"
    control.tar.gz   ./control, ./conffiles and the maintainer scripts
    data.tar.gz      the payload, paths relative to the filesystem root

This script writes those bytes directly (no dpkg-deb, fakeroot, tar or ar
binaries needed), so it runs the same way on Windows and Linux.  The version
comes from Cargo.toml [package].version, which stays the single source of truth.

Package layout:
    /usr/bin/psk-server
    /usr/bin/psk-client
    /usr/lib/systemd/system/psk-quic-proxy.service
    /etc/psk-quic-proxy/server.env            (conffile, mode 0600)
    /usr/share/doc/psk-quic-proxy/README.md
    /usr/share/doc/psk-quic-proxy/VERSION
    /usr/share/doc/psk-quic-proxy/copyright

Usage:
    python3 scripts/build_deb.py --bin-dir target/release --out-dir dist
    python3 scripts/build_deb.py --self-test
"""

from __future__ import annotations

import argparse
import gzip
import io
import os
import re
import sys
import tarfile

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PACKAGE = "psk-quic-proxy"
DOC_DIR = "usr/share/doc/" + PACKAGE
UNIT_PATH = "usr/lib/systemd/system/psk-quic-proxy.service"
ENV_PATH = "etc/psk-quic-proxy/server.env"

MODE_EXEC = 0o755
MODE_FILE = 0o644
# server.env may carry the PSK; keep it root-only for the same reason fcp did:
# systemd (PID 1, root) parses EnvironmentFile and injects the value, so the
# service user never needs to read the file.
MODE_CONFFILE = 0o600
OWNER = "root"
GROUP = "root"

VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
ARCHES = {"amd64", "arm64", "i386", "armhf", "riscv64", "ppc64el", "s390x"}

# rustls/ring are statically linked, so there is no OpenSSL dependency.
DEFAULT_DEPENDS = "libc6 (>= 2.35), libgcc-s1"
SUMMARY = "QUIC proxy server and client with PSK authentication"


class PackagingError(Exception):
    pass


def log(msg: str) -> None:
    print(msg, flush=True)


def read_text(path: str) -> str:
    with open(path, "r", encoding="utf-8") as handle:
        return handle.read()


def write_text(path: str, text: str) -> None:
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)


def read_version(repo: str) -> str:
    """Read [package].version from Cargo.toml without a TOML dependency."""
    path = os.path.join(repo, "Cargo.toml")
    if not os.path.isfile(path):
        raise PackagingError(f"missing {path}")
    in_package = False
    for line in read_text(path).splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_package = stripped == "[package]"
            continue
        if in_package:
            match = re.match(r'^version\s*=\s*"([^"]+)"', stripped)
            if match:
                version = match.group(1)
                if not VERSION_RE.match(version):
                    raise PackagingError(
                        f"Cargo.toml version must be MAJOR.MINOR.PATCH, got {version!r}"
                    )
                return version
    raise PackagingError("Cargo.toml [package] has no version")


def metadata_value(repo: str, key: str) -> str:
    path = os.path.join(repo, "packaging", "debian", "metadata.env")
    if not os.path.isfile(path):
        raise PackagingError(f"missing {path}")
    for line in read_text(path).splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, value = line.split("=", 1)
        if name.strip() == key:
            return value.strip()
    raise PackagingError(f"{path} has no {key}=")


def normalize_description(text: str) -> list[str]:
    """Translate the human-readable description into deb-control(5) lines."""
    lines = [line.strip() for line in text.strip().splitlines()]
    if not lines:
        raise PackagingError("packaging/debian/description is empty")
    return [line if line else "." for line in lines]


def build_control(repo: str, version: str, arch: str, depends: str) -> str:
    homepage = metadata_value(repo, "PSK_HOMEPAGE")
    maintainer = metadata_value(repo, "PSK_MAINTAINER")
    description = normalize_description(
        read_text(os.path.join(repo, "packaging/debian/description"))
    )

    lines = [
        f"Package: {PACKAGE}",
        f"Version: {version}",
        f"Architecture: {arch}",
        f"Maintainer: {maintainer}",
        "Installed-Size: {installed_size}",
        f"Depends: {depends}",
        "Section: net",
        "Priority: optional",
        f"Homepage: {homepage}",
        f"Description: {SUMMARY}",
    ]
    lines += [f" {line}" for line in description]
    # Exactly one trailing newline: dpkg splits paragraphs on blank lines.
    return "\n".join(lines) + "\n"


def iter_package_files(
    repo: str, bin_dir: str, allow_non_linux: bool
) -> list[tuple[str, str, int]]:
    """(archive path, source path, mode), in data.tar order."""
    files: list[tuple[str, str, int]] = []

    for name in ("psk-server", "psk-client"):
        src = os.path.join(bin_dir, name)
        exe = src + ".exe"
        if os.path.isfile(src):
            files.append((f"usr/bin/{name}", src, MODE_EXEC))
        elif os.path.isfile(exe):
            if not allow_non_linux:
                raise PackagingError(
                    f"{exe} is a Windows executable and cannot run on Linux. "
                    "Build on Linux (the release workflow's linux-deb job) or pass "
                    "--allow-non-linux-bins to only inspect the package structure."
                )
            files.append((f"usr/bin/{name}", exe, MODE_EXEC))
        else:
            raise PackagingError(
                f"cannot find {name} (or {name}.exe) in {bin_dir}; "
                "run cargo build --release first"
            )

    for src, dst, mode in (
        (os.path.join(repo, "packaging/debian/psk-quic-proxy.service"), UNIT_PATH, MODE_FILE),
        (os.path.join(repo, "packaging/debian/server.env"), ENV_PATH, MODE_CONFFILE),
        (os.path.join(repo, "packaging/debian/copyright"), f"{DOC_DIR}/copyright", MODE_FILE),
        (os.path.join(repo, "README.md"), f"{DOC_DIR}/README.md", MODE_FILE),
    ):
        if not os.path.isfile(src):
            raise PackagingError(f"missing {src}")
        files.append((dst, src, mode))

    return files


def add_file(tar: tarfile.TarFile, arcname: str, src: str, mode: int) -> None:
    info = tarfile.TarInfo(arcname)
    info.type = tarfile.REGTYPE
    info.size = os.path.getsize(src)
    info.mode = mode
    info.uid = 0
    info.gid = 0
    info.uname = OWNER
    info.gname = GROUP
    info.mtime = 0  # deterministic: identical input -> identical bytes
    with open(src, "rb") as handle:
        tar.addfile(info, handle)


def add_dir(tar: tarfile.TarFile, arcname: str) -> None:
    info = tarfile.TarInfo(arcname)
    info.type = tarfile.DIRTYPE
    info.mode = MODE_EXEC
    info.uid = 0
    info.gid = 0
    info.uname = OWNER
    info.gname = GROUP
    info.mtime = 0
    tar.addfile(info)


def add_bytes(tar: tarfile.TarFile, arcname: str, data: bytes, mode: int) -> None:
    info = tarfile.TarInfo(arcname)
    info.size = len(data)
    info.mode = mode
    info.uid = 0
    info.gid = 0
    info.uname = OWNER
    info.gname = GROUP
    info.mtime = 0
    tar.addfile(info, io.BytesIO(data))


def dirs_of(files: list[tuple[str, str, int]]) -> list[str]:
    seen: set[str] = set()
    for arc, _, _ in files:
        parts = arc.split("/")[:-1]
        for index in range(1, len(parts) + 1):
            seen.add("/".join(parts[:index]))
    return sorted(seen)


def build_data_tar(files: list[tuple[str, str, int]], version_file: str) -> tuple[bytes, int]:
    entries: list[tuple[str, str | None, int]] = [
        (name + "/", None, MODE_EXEC) for name in dirs_of(files)
    ]
    entries += [(arc, src, mode) for arc, src, mode in files]
    entries.append((f"{DOC_DIR}/VERSION", version_file, MODE_FILE))
    entries.sort(key=lambda item: item[0])

    raw = io.BytesIO()
    total = 0
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tar:
        for arc, src, mode in entries:
            if src is None:
                add_dir(tar, arc)
            else:
                add_file(tar, arc, src, mode)
                total += os.path.getsize(src)
    return gzip.compress(raw.getvalue(), mtime=0), total


def build_control_tar(repo: str, control: str) -> bytes:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT) as tar:
        add_bytes(tar, "./control", control.encode("utf-8"), MODE_FILE)
        add_bytes(tar, "./conffiles", f"/{ENV_PATH}\n".encode("utf-8"), MODE_FILE)
        for script in ("postinst", "prerm", "postrm"):
            src = os.path.join(repo, "packaging/debian", script)
            if not os.path.isfile(src):
                raise PackagingError(f"missing {src}")
            with open(src, "rb") as handle:
                add_bytes(tar, "./" + script, handle.read(), MODE_EXEC)
    return gzip.compress(raw.getvalue(), mtime=0)


def ar_member(name: str, data: bytes) -> bytes:
    if len(name) > 16:
        raise PackagingError(f"ar member name too long: {name}")
    header = "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n".format(
        name, "0", "0", "0", MODE_FILE, len(data)
    ).encode("ascii")
    if len(header) != 60:
        raise PackagingError(f"ar header is {len(header)} bytes, expected 60")
    return header + data + (b"\n" if len(data) % 2 else b"")


def ar_archive(members: list[tuple[str, bytes]]) -> bytes:
    out = b"!<arch>\n"
    for name, data in members:
        out += ar_member(name, data)
    return out


def verify_deb(deb_path: str, control: str, files: list[tuple[str, str, int]]) -> None:
    with open(deb_path, "rb") as handle:
        blob = handle.read()
    if not blob.startswith(b"!<arch>\n"):
        raise PackagingError("not an ar archive")

    members: dict[str, bytes] = {}
    offset = 8
    while offset + 60 <= len(blob):
        header = blob[offset : offset + 60]
        name = header[:16].decode("ascii").rstrip()
        size = int(header[48:58].decode("ascii").strip())
        offset += 60
        members[name] = blob[offset : offset + size]
        offset += size + (size % 2)
    for need in ("debian-binary", "control.tar.gz", "data.tar.gz"):
        if need not in members:
            raise PackagingError(f"deb is missing member {need}")
    if members["debian-binary"] != b"2.0\n":
        raise PackagingError("debian-binary must be '2.0\\n'")

    with tarfile.open(fileobj=io.BytesIO(gzip.decompress(members["control.tar.gz"]))) as tar:
        names = tar.getnames()
        control_file = tar.extractfile("./control")
        if control_file is None:
            raise PackagingError("control.tar has no ./control")
        got = control_file.read().decode("utf-8")
    if got != control:
        raise PackagingError("control readback does not match what was written")
    if "\n\n" in got:
        raise PackagingError("control contains a blank line (dpkg would split the paragraph)")
    for field in ("Package:", "Version:", "Architecture:", "Maintainer:", "Description:"):
        if field not in got:
            raise PackagingError(f"control is missing {field}")
    if "./conffiles" not in names:
        raise PackagingError("control.tar is missing conffiles")

    with tarfile.open(fileobj=io.BytesIO(gzip.decompress(members["data.tar.gz"]))) as tar:
        data_names = set(tar.getnames())
    for arc, _, _ in files:
        if arc not in data_names:
            raise PackagingError(f"data.tar is missing {arc}")


def self_test() -> int:
    failures: list[str] = []

    def check(name: str, ok: bool) -> None:
        print(f"{'PASS' if ok else 'FAIL'}  {name}")
        if not ok:
            failures.append(name)

    desc = normalize_description("Summary line\nsecond\n\nfourth\n")
    check("description blank line -> '.'", desc == ["Summary line", "second", ".", "fourth"])

    member = ar_member("debian-binary", b"2.0\n")
    check("ar header is 60 bytes", len(member) - 4 == 60)
    check("ar name field", member[:16].decode().rstrip() == "debian-binary")
    check("ar even length is not padded", len(member) == 64)
    odd = ar_member("x", b"2.0")
    check("ar odd length is padded", len(odd) == 64 and odd.endswith(b"2.0\n"))
    check("ar magic", ar_archive([]) == b"!<arch>\n")

    check("version regex accepts 0.1.0", bool(VERSION_RE.match("0.1.0")))
    check("version regex rejects v0.1.0", not VERSION_RE.match("v0.1.0"))

    dirs = dirs_of([("usr/bin/a", "x", MODE_EXEC), ("usr/bin/b", "y", MODE_EXEC)])
    check("parent directories deduplicated", dirs == ["usr", "usr/bin"])

    if failures:
        print(f"::error::{len(failures)} self-test(s) failed", file=sys.stderr)
        return 1
    print("build_deb self-test passed")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Build the psk-quic-proxy .deb")
    parser.add_argument("--self-test", action="store_true", help="run only the pure-logic checks")
    parser.add_argument("--repo", default=REPO_ROOT, help="repository root")
    parser.add_argument("--bin-dir", help="directory holding psk-server / psk-client")
    parser.add_argument("--out-dir", default=None, help="output directory (default: <repo>/dist)")
    parser.add_argument("--version", default=None, help="override the Cargo.toml version (CI passes the tag)")
    parser.add_argument("--arch", default="amd64", help="Debian architecture name")
    parser.add_argument("--depends", default=DEFAULT_DEPENDS)
    parser.add_argument("--allow-non-linux-bins", action="store_true",
                        help="allow packaging .exe files (structure checks only)")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    repo = os.path.abspath(args.repo)
    try:
        version = args.version or read_version(repo)
        if not VERSION_RE.match(version):
            raise PackagingError(f"--version must be MAJOR.MINOR.PATCH, got {version!r}")
        if args.arch not in ARCHES:
            raise PackagingError(f"unknown arch {args.arch}; choose from {', '.join(sorted(ARCHES))}")
        if not args.bin_dir:
            raise PackagingError("--bin-dir is required (or use --self-test)")
        bin_dir = os.path.abspath(args.bin_dir)
        if not os.path.isdir(bin_dir):
            raise PackagingError(f"--bin-dir does not exist: {bin_dir}")

        out_dir = os.path.abspath(args.out_dir or os.path.join(repo, "dist"))
        os.makedirs(out_dir, exist_ok=True)

        # A small VERSION doc inside the package (the binary has no --version
        # string of its own beyond clap's).
        version_file = os.path.join(out_dir, ".VERSION-doc")
        write_text(version_file, version + "\n")

        files = iter_package_files(repo, bin_dir, args.allow_non_linux_bins)
        data_gz, data_bytes = build_data_tar(files, version_file)
        installed_kib = (data_bytes + 1023) // 1024

        control = build_control(repo, version, args.arch, args.depends).replace(
            "{installed_size}", str(installed_kib)
        )
        control_gz = build_control_tar(repo, control)

        deb = ar_archive([
            ("debian-binary", b"2.0\n"),
            ("control.tar.gz", control_gz),
            ("data.tar.gz", data_gz),
        ])
        deb_path = os.path.join(out_dir, f"{PACKAGE}_{version}_{args.arch}.deb")
        with open(deb_path, "wb") as handle:
            handle.write(deb)

        os.remove(version_file)

        log("== deb structure self-check")
        verify_deb(deb_path, control, files)
        log("== control")
        log(control.rstrip("\n"))
        log(f"\n== wrote {deb_path} ({len(deb)} bytes, Installed-Size {installed_kib} KiB)")
        for arc, _, _ in files:
            log(f"   /{arc}")
        return 0
    except PackagingError as exc:
        print(f"::error::{exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
