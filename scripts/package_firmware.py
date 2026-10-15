#!/usr/bin/env python3
"""Package and validate a 1004 bridge ELF without programming any hardware."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import zlib

MAGIC = b"BZM-BRIDGE-FW\0\0\0"


def inspect_image(data):
    offset = data.find(MAGIC)
    if offset < 0 or data.find(MAGIC, offset + 1) >= 0:
        raise ValueError("image must contain exactly one bridge manifest")
    if not 256 < len(data) <= 2 * 1024 * 1024:
        raise ValueError("invalid RP2040 flash image size")
    manifest = data[offset:offset + 96]
    if len(manifest) != 96 or manifest[16:18] != bytes([1, 96]):
        raise ValueError("invalid manifest schema or size")
    if int.from_bytes(manifest[18:20], "little") != 1004:
        raise ValueError("image does not target board 1004")
    if manifest[20:23] != bytes([1, 3, 0]):
        raise ValueError("image must be bridge firmware with protocol 3.0")
    if zlib.crc32(manifest[:92]) != int.from_bytes(manifest[92:], "little"):
        raise ValueError("invalid manifest CRC")
    length = manifest[23]
    version = manifest[24:24 + length]
    if not 1 <= length <= 63 or not all(0x20 <= byte <= 0x7e for byte in version):
        raise ValueError("invalid firmware version")
    if any(manifest[24 + length:92]):
        raise ValueError("nonzero manifest padding")
    return {
        "board": 1004,
        "protocol": "3.0",
        "version": version.decode("ascii"),
        "manifest_offset": offset,
        "manifest_crc_valid": True,
        "size": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }


def find_objcopy():
    for name in ["llvm-objcopy", "arm-none-eabi-objcopy"]:
        if found := shutil.which(name):
            return found
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    candidates = list((sysroot / "lib/rustlib").glob("*/bin/llvm-objcopy"))
    if len(candidates) != 1:
        raise ValueError("install llvm-tools with rustup or pass --objcopy")
    return str(candidates[0])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--elf", type=Path, default=Path("target/thumbv6m-none-eabi/release/bonanza-bridge-fw"))
    parser.add_argument("--out", type=Path, default=Path("dist"))
    parser.add_argument("--objcopy")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    # Validate before replacing a previously packaged firmware image.
    temporary = args.out / "bonanza-bridge-fw-1004.bin.tmp"
    try:
        subprocess.run([args.objcopy or find_objcopy(), "-O", "binary", str(args.elf), str(temporary)], check=True)
        identity = inspect_image(temporary.read_bytes())
        temporary.replace(args.out / "bonanza-bridge-fw-1004.bin")
    finally:
        temporary.unlink(missing_ok=True)
    shutil.copy2(args.elf, args.out / "bonanza-bridge-fw-1004.elf")
    (args.out / "bonanza-bridge-fw-1004.json").write_text(json.dumps(identity, indent=2) + "\n")
    print(json.dumps(identity, indent=2))


if __name__ == "__main__":
    main()
