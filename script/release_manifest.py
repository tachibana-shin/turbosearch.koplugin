#!/usr/bin/env python3
"""Writes an index of the release archives: platform, target, size, checksum.

A program rather than a Makefile recipe, because a heredoc inside a recipe is not
supported and because this does something Make has no opinion about: reads every
archive in the build directory and decides what each one is.

Attached alongside the archives in the release, so a script — or a reader deciding
whether to redeploy — can tell what it has without unpacking anything.
"""

import hashlib
import json
import os
import sys

# What each platform is, said the way a reader would need it said.
PLATFORMS = {
    "desktop": (
        "x86_64-unknown-linux-gnu",
        "The desktop reader, and any x86-64 machine",
    ),
    "kindle": (
        "armv7-unknown-linux-gnueabihf",
        "Kindle Paperwhite 3, ARMv7 hard-float",
    ),
    "aarch64": (
        "aarch64-unknown-linux-gnu",
        "Newer readers with a 64-bit ARM",
    ),
}

# The prefix every archive starts with, and the one it does not.
ARCHIVE_PREFIX = "turbosearch-"
ARCHIVE_SUFFIX = ".zip"


def archive_key(name):
    """The platform an archive is for, or None if it is not one of ours."""
    if not name.startswith(ARCHIVE_PREFIX) or not name.endswith(ARCHIVE_SUFFIX):
        return None
    # The version sits between the platform and the suffix, and a version can hold
    # dashes of its own, so the platform is whichever prefix actually matches.
    stem = name[len(ARCHIVE_PREFIX) : -len(ARCHIVE_SUFFIX)]
    for key in PLATFORMS:
        if stem == key or stem.startswith(key + "-"):
            return key
    return None


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 16), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main(argv):
    out = argv[1] if len(argv) > 1 else "build/turbosearch.json"
    here = os.path.dirname(out)
    version = os.environ.get("VERSION", "0.0.0")

    assets = []
    for name in sorted(os.listdir(here)):
        if archive_key(name) is None:
            continue
        key = archive_key(name)
        target, note = PLATFORMS[key]
        path = os.path.join(here, name)
        assets.append(
            {
                "platform": key,
                "target": target,
                "file": name,
                "bytes": os.path.getsize(path),
                "sha256": sha256(path),
                "note": note,
            }
        )

    manifest = {
        "name": "turbosearch",
        "version": version,
        "assets": assets,
    }
    with open(out, "w", encoding="utf-8") as handle:
        json.dump(manifest, handle, indent=2)
        handle.write("\n")
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
