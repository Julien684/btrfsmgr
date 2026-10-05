#!/usr/bin/env python3
"""
build_eopkg.py — Builds a .eopkg for btrfsmgr in the exact Solus binary format.

The output is a ZIP containing:
  - metadata.xml
  - files.xml
  - install.tar.xz

No COMAR scripts (pure file package, no install/uninstall hooks needed).
"""
import hashlib
import io
import os
import stat
import sys
import tarfile
import zipfile
from datetime import date
from pathlib import Path

# ──────────────────────────────────────────────────────────────────────────────
# Configuration
# ──────────────────────────────────────────────────────────────────────────────
SRC_DIR = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")
OUT_DIR = Path(sys.argv[2]) if len(sys.argv) > 2 else SRC_DIR

PACKAGE_NAME   = "btrfsmgr"
VERSION        = "1.0.4"   # must match VERSION in btrfsmgr.py
RELEASE        = 1
DIST_RELEASE   = 1
ARCH           = "x86_64"
DIST_NAME      = "Solus"
BUILD_HOST     = "solus-local"
HOME_PAGE      = "https://github.com/Julien684/btrfsmgr"
PACKAGER_NAME  = "Julien"
PACKAGER_EMAIL = "julien@getsol.us"
PART_OF        = "system.utilities"

# Files to include in the package.
# Each entry: (relative_path_in_root, source_path_relative_to_SRC_DIR, mode)
#
# On Solus, /bin is a symlink to /usr/bin, so we install to usr/bin.
FILE_LIST = [
    # (dest_path, src_rel_path, mode)
    ("usr/bin/btrfsmgr",                          "btrfsmgr.py",         0o755),
    ("usr/share/applications/btrfsmgr.desktop",   "btrfsmgr.desktop",    0o644),
    ("usr/share/doc/btrfsmgr/README.md",          "README.md",           0o644),
    ("usr/share/licenses/btrfsmgr/LICENSE",       "LICENSE",             0o644),
]

# Runtime dependencies (package names; release=0 means "any release")
RUNTIME_DEPS = [
    "btrfs-progs",
    "python3",
    "systemd",
]

OUT_FILENAME = f"{PACKAGE_NAME}-{VERSION}-{RELEASE}-{DIST_RELEASE}-{ARCH}.eopkg"


# ──────────────────────────────────────────────────────────────────────────────
# Helpers
# ──────────────────────────────────────────────────────────────────────────────
def sha1_hex(data: bytes) -> str:
    return hashlib.sha1(data).hexdigest()


def file_type_for(path: str) -> str:
    """Return the eopkg file type for a given absolute path (no leading /)."""
    if path.startswith("usr/bin/") or path.startswith("bin/"):
        return "executable"
    if path.startswith("usr/libexec/") or path.startswith("usr/sbin/") or path.startswith("sbin/"):
        return "executable"
    if path.startswith("usr/include/"):
        return "header"
    if path.startswith("usr/lib/"):
        return "library"
    if path.startswith("usr/share/man/"):
        return "man"
    if path.startswith("usr/share/info/"):
        return "info"
    if path.startswith("usr/share/locale/"):
        return "localedata"
    if path.startswith("usr/share/doc/") or path.startswith("usr/share/gtk-doc/") or path.startswith("usr/share/help/"):
        return "doc"
    if path.startswith("usr/lib/pkgconfig/") or path.startswith("usr/lib32/pkgconfig/") or path.startswith("usr/lib64/pkgconfig/"):
        return "data"
    if path.startswith("etc/"):
        return "config"
    return "data"


# ──────────────────────────────────────────────────────────────────────────────
# Build
# ──────────────────────────────────────────────────────────────────────────────
def build():
    out_path = OUT_DIR / OUT_FILENAME
    print(f"Building: {out_path}")

    # ── Collect file contents ────────────────────────────────────────────────
    file_entries = []  # list of (dest_path, bytes, mode)
    for dest_path, src_rel, mode in FILE_LIST:
        src = SRC_DIR / src_rel
        if not src.exists():
            print(f"  WARNING: source file not found: {src}", file=sys.stderr)
            continue
        data = src.read_bytes()
        file_entries.append((dest_path, data, mode))
        print(f"  + {dest_path}  ({len(data)} bytes, mode {oct(mode)})")

    # ── Build install.tar.xz ─────────────────────────────────────────────────
    # Only regular files, no directory entries, paths relative (no ./)
    tar_buf = io.BytesIO()
    with tarfile.open(fileobj=tar_buf, mode="w:xz") as tar:
        for dest_path, data, mode in file_entries:
            info = tarfile.TarInfo(name=dest_path)
            info.size = len(data)
            info.mode = mode
            info.uid = 0
            info.gid = 0
            info.mtime = 0
            info.type = tarfile.REGTYPE
            tar.addfile(info, io.BytesIO(data))
    install_tar_xz = tar_buf.getvalue()
    print(f"  install.tar.xz: {len(install_tar_xz)} bytes")

    # ── Build files.xml ──────────────────────────────────────────────────────
    files_parts = ['<Files>']
    for dest_path, data, mode in file_entries:
        ftype = file_type_for(dest_path)
        mode_str = f"{mode:04o}"
        h = sha1_hex(data)
        files_parts.append("  <File>")
        files_parts.append(f"    <Path>{dest_path}</Path>")
        files_parts.append(f"    <Type>{ftype}</Type>")
        files_parts.append(f"    <Size>{len(data)}</Size>")
        files_parts.append(f"    <Uid>0</Uid>")
        files_parts.append(f"    <Gid>0</Gid>")
        files_parts.append(f"    <Mode>{mode_str}</Mode>")
        files_parts.append(f"    <Hash>{h}</Hash>")
        files_parts.append("  </File>")
    files_parts.append("</Files>")
    files_xml = "\n".join(files_parts).encode("utf-8")
    print(f"  files.xml: {len(files_xml)} bytes")

    # ── Build metadata.xml ───────────────────────────────────────────────────
    today = date.today().isoformat()
    installed_size = sum(len(data) for _, data, _ in file_entries)

    deps_xml = ""
    if RUNTIME_DEPS:
        deps_xml = "    <RuntimeDependencies>\n"
        for dep in RUNTIME_DEPS:
            deps_xml += f'        <Dependency release="0">{dep}</Dependency>\n'
        deps_xml += "    </RuntimeDependencies>\n"

    def a(s: str) -> str:
        return s.encode("ascii", "replace").decode("ascii")

    metadata_xml = f"""<PISI>
    <Source>
        <Name>{PACKAGE_NAME}</Name>
        <Homepage>{HOME_PAGE}</Homepage>
        <Packager>
            <Name>{PACKAGER_NAME}</Name>
            <Email>{PACKAGER_EMAIL}</Email>
        </Packager>
    </Source>
    <Package>
        <Name>{PACKAGE_NAME}</Name>
        <Summary xml:lang="en">{a("BTRFS snapshot manager TUI - create, list, restore, and automate BTRFS subvolume snapshots")}</Summary>
        <Description xml:lang="en">{a("""btrfsmgr is a TUI tool for managing BTRFS subvolume snapshots on Linux.
It supports creating, listing, restoring, and deleting snapshots; scheduling
automatic snapshots via systemd timers with a retention policy; and generating
boot entries (GRUB / systemd-boot) so you can boot directly into a snapshot.
Restore is a CoW swap (two renames) - no data is copied.
Requires btrfs-progs and Python 3.8+ (standard library only, no third-party
dependencies). Unofficial; not affiliated with Solus or the BTRFS project.""")}</Description>
        <PartOf>{PART_OF}</PartOf>
        <License>GPL-3.0-or-later</License>
{deps_xml}        <History>
            <Update release="{RELEASE}">
                <Date>{today}</Date>
                <Version>{VERSION}</Version>
                <Comment>{PACKAGE_NAME} {VERSION} initial package</Comment>
                <Name>{PACKAGER_NAME}</Name>
                <Email>{PACKAGER_EMAIL}</Email>
            </Update>
        </History>
        <BuildHost>{BUILD_HOST}</BuildHost>
        <Distribution>{DIST_NAME}</Distribution>
        <DistributionRelease>{DIST_RELEASE}</DistributionRelease>
        <Architecture>{ARCH}</Architecture>
        <InstalledSize>{installed_size}</InstalledSize>
        <PackageFormat>1.2</PackageFormat>
        <Source>
            <Name>{PACKAGE_NAME}</Name>
            <Homepage>{HOME_PAGE}</Homepage>
            <Packager>
                <Name>{PACKAGER_NAME}</Name>
                <Email>{PACKAGER_EMAIL}</Email>
            </Packager>
        </Source>
    </Package>
</PISI>""".encode("utf-8")
    print(f"  metadata.xml: {len(metadata_xml)} bytes")

    # ── Assemble ZIP ─────────────────────────────────────────────────────────
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        zf.writestr("metadata.xml", metadata_xml)
        zf.writestr("files.xml", files_xml)
        zf.writestr("install.tar.xz", install_tar_xz)

    size = out_path.stat().st_size
    print(f"\n  ✓ {out_path}  ({size} bytes)")
    print(f"  SHA1: {sha1_hex(out_path.read_bytes())}")
    return str(out_path)


if __name__ == "__main__":
    build()
