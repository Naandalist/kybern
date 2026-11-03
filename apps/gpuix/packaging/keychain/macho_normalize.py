"""Print a SHA-256 of a Mach-O (thin or universal) with the two fields a rebuild cannot reproduce set to zero:
LC_UUID (the linker derives it from the temporary object paths) and the __LINKEDIT segment's vmsize (left over from the
code signature that `codesign --remove-signature` took out). Every other byte counts: the header, every load command,
all of __TEXT and __DATA, and the symbol and fixup tables. Run with `python3 -I`; see reproduce.sh."""
import hashlib
import struct
import sys

LC_SEGMENT_64, LC_UUID, LC_CODE_SIGNATURE = 0x19, 0x1B, 0x1D


def normalize_thin(data: bytearray, base: int) -> None:
    magic, = struct.unpack_from("<I", data, base)
    if magic != 0xFEEDFACF:
        raise SystemExit(f"not a 64-bit Mach-O slice at {base}")
    ncmds, = struct.unpack_from("<I", data, base + 16)
    at = base + 32
    for _ in range(ncmds):
        cmd, size = struct.unpack_from("<II", data, at)
        if cmd == LC_CODE_SIGNATURE:
            raise SystemExit("the binary still carries a code signature; remove it first")
        if cmd == LC_UUID:
            data[at + 8:at + 24] = bytes(16)
        if cmd == LC_SEGMENT_64 and bytes(data[at + 8:at + 24]).rstrip(b"\0") == b"__LINKEDIT":
            data[at + 32:at + 40] = bytes(8)
        at += size


def normalized(path: str) -> bytes:
    data = bytearray(open(path, "rb").read())
    magic, = struct.unpack_from(">I", data, 0)
    if magic == 0xCAFEBABE:
        count, = struct.unpack_from(">I", data, 4)
        for index in range(count):
            _cpu, _sub, offset, _size, _align = struct.unpack_from(">iiIII", data, 8 + index * 20)
            normalize_thin(data, offset)
    else:
        normalize_thin(data, 0)
    return bytes(data)


if __name__ == "__main__":
    for path in sys.argv[1:]:
        print(hashlib.sha256(normalized(path)).hexdigest(), path)
