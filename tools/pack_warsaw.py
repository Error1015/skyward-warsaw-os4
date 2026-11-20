"""Pack a Skyward recovery image, as `skyward-pack recovery` does.

Reads the stock recovery image, replaces the recovery program in its
ramdisk, adds the files the recovery carries, packs the ramdisk the way
AOSP does (newc, LZ4 legacy framing) and ends the image in the same
unsigned AVB footer `skyward-pack` lays.
"""
import hashlib
import os
import struct
import subprocess
import sys
import tempfile

S_IFMT = 0o170000
S_IFDIR = 0o040000
S_IFREG = 0o100000
S_IFLNK = 0o120000

BLOCK = 4096
FOOTER_LEN = 64
HEADER_LEN = 256
TAG_HASH = 2
RELEASE = b"skyward-pack"

BUILD_VERSION = (
    "ro.build.version.release",
    "ro.build.version.security_patch",
    "ro.vendor.build.security_patch",
)

PROPS = b"\nro.secure=0\nro.adb.secure=0\nro.debuggable=1\nro.ota.allow_downgrade=true\n"


# ---------------------------------------------------------------- ramdisk


class Entry:
    def __init__(self, name, mode, uid, gid, mtime, nlink, dev, rdev, data):
        self.name, self.mode, self.uid, self.gid = name, mode, uid, gid
        self.mtime, self.nlink, self.dev, self.rdev, self.data = mtime, nlink, dev, rdev, data

    def is_dir(self):
        return self.mode & S_IFMT == S_IFDIR


def decode_cpio(data):
    entries, pos = [], 0
    while True:
        if data[pos:pos + 6] != b"070701":
            raise SystemExit("bad cpio magic at %d" % pos)
        f = lambda i: int(data[pos + 6 + 8 * i:pos + 6 + 8 * (i + 1)], 16)
        ino, mode, uid, gid, nlink, mtime, size = f(0), f(1), f(2), f(3), f(4), f(5), f(6)
        dev = (f(7), f(8))
        rdev = (f(9), f(10))
        namesize = f(11)
        pos += 110
        name = data[pos:pos + namesize - 1].decode("utf-8", "surrogateescape")
        pos = (pos + namesize + 3) & ~3
        body = data[pos:pos + size]
        pos = (pos + size + 3) & ~3
        if name == "TRAILER!!!":
            return entries
        entries.append(Entry(name, mode, uid, gid, mtime, nlink, dev, rdev, body))


def encode_cpio(entries):
    out = bytearray()

    def field(n):
        return b"%08x" % (n & 0xFFFFFFFF)

    for ino, e in enumerate(entries):
        name = e.name.encode("utf-8", "surrogateescape") + b"\0"
        out += b"070701"
        out += field(300000 + ino)
        for v in (e.mode, e.uid, e.gid, e.nlink, e.mtime, len(e.data), e.dev[0], e.dev[1], e.rdev[0], e.rdev[1]):
            out += field(v)
        out += field(len(name))
        out += field(0)
        out += name
        out = bytearray(out)
        while len(out) % 4:
            out += b"\0"
        out += e.data
        while len(out) % 4:
            out += b"\0"
    name = b"TRAILER!!!\0"
    out += b"070701"
    out += field(0) * 8
    out += field(0)
    out += field(0) * 2
    out += field(len(name))
    out += field(0)
    out += name
    while len(out) % 512:
        out += b"\0"
    return bytes(out)


def root_entry(name, mode, data):
    return Entry(name, mode, 0, 0, 0, 2 if mode & S_IFMT == S_IFDIR else 1, (0, 0), (0, 0), data)


def find(entries, name):
    for e in entries:
        if e.name == name:
            return e
    return None


def add(entries, name, mode, data):
    parts = name.split("/")
    parent = ""
    for part in parts[:-1]:
        parent = part if not parent else parent + "/" + part
        there = find(entries, parent)
        if there is None:
            entries.append(root_entry(parent, S_IFDIR | 0o755, b""))
        elif not there.is_dir():
            raise SystemExit("%s is not a directory" % parent)
    if find(entries, name) is not None:
        raise SystemExit("%s is already in the ramdisk" % name)
    entries.append(root_entry(name, mode, data))


def replace(entries, name, data):
    e = find(entries, name)
    if e is None or e.mode & S_IFMT != S_IFREG:
        raise SystemExit("%s is not a regular file in the ramdisk" % name)
    e.data = data


def without(data, names):
    out = []
    for line in data.splitlines(keepends=True):
        key = line.split(b"=")[0]
        if key.decode("utf-8", "replace") not in names:
            out.append(line)
    return b"".join(out)


def lz4_legacy(raw, lz4):
    """`lz4 -l -12 --favor-decSpeed -c`, as skyward-bootimg packs it."""
    with tempfile.NamedTemporaryFile(delete=False, suffix=".cpio") as f:
        f.write(raw)
        path = f.name
    try:
        out = subprocess.run([lz4, "-l", "-12", "--favor-decSpeed", "-c", path], check=True, capture_output=True)
    finally:
        os.unlink(path)
    return out.stdout


# -------------------------------------------------------------------- avb


def hash_descriptor(image_size, partition, salt, digest):
    algorithm = b"sha256" + b"\0" * 26
    d = bytearray()
    d += struct.pack(">Q", TAG_HASH)
    d += struct.pack(">Q", 0)
    d += struct.pack(">Q", image_size)
    d += algorithm
    for n in (len(partition), len(salt), len(digest), 0):
        d += struct.pack(">I", n)
    d += b"\0" * 60
    d += partition.encode()
    d += salt
    d += digest
    while len(d) % 8:
        d += b"\0"
    struct.pack_into(">Q", d, 8, len(d) - 16)
    return bytes(d)


def vbmeta(descriptors):
    aux = (len(descriptors) + 63) // 64 * 64
    v = bytearray()
    v += b"AVB0"
    v += struct.pack(">I", 1)
    v += struct.pack(">I", 0)
    v += struct.pack(">Q", 0)
    v += struct.pack(">Q", aux)
    v += struct.pack(">I", 0)
    n = len(descriptors)
    for field in (0, 0, 0, 0, n, 0, n, 0, 0, n, 0):
        v += struct.pack(">Q", field)
    v += struct.pack(">I", 0)
    v += struct.pack(">I", 0)
    v += RELEASE
    v += b"\0" * (HEADER_LEN - len(v))
    v += descriptors
    v += b"\0" * (HEADER_LEN + aux - len(v))
    return bytes(v)


def footed(image, partition, partition_size, salt):
    digest = hashlib.sha256(salt + image).digest()
    vbm = vbmeta(hash_descriptor(len(image), partition, salt, digest))
    offset = (len(image) + BLOCK - 1) // BLOCK * BLOCK
    end = offset + (len(vbm) + BLOCK - 1) // BLOCK * BLOCK
    if end + BLOCK > partition_size:
        raise SystemExit("the image and its vbmeta do not fit the partition")
    out = bytearray(image)
    out += b"\0" * (offset - len(out))
    out += vbm
    out += b"\0" * (partition_size - FOOTER_LEN - len(out))
    out += b"AVBf"
    out += struct.pack(">I", 1)
    out += struct.pack(">I", 0)
    for n in (len(image), offset, len(vbm)):
        out += struct.pack(">Q", n)
    out += b"\0" * (partition_size - len(out))
    return bytes(out)


def self_test():
    image = b"S" * 4096
    got = footed(image, "recovery", 81920, b"\x5a" * 32)
    want = open(os.path.join(HERE, "..", "crates/skyward-pack/tests/avb-footer-4096.bin"), "rb").read()
    if got != want:
        raise SystemExit("the footer is not avbtool's")
    print("avb footer: byte for byte avbtool's")


# ------------------------------------------------------------------- pack


def main():
    stock, out = sys.argv[1], sys.argv[2]
    lz4 = sys.argv[3]
    raw = open(stock, "rb").read()
    header = bytearray(raw[:BLOCK])
    if header[:8] != b"ANDROID!":
        raise SystemExit("not an Android boot image")
    ramdisk_size = struct.unpack_from("<I", header, 12)[0]
    compressed = raw[BLOCK:BLOCK + ramdisk_size]
    cpio = decompress(compressed)
    entries = decode_cpio(cpio)
    print("ramdisk: %d entries, %d bytes" % (len(entries), len(cpio)))

    replace(entries, "system/bin/recovery", read("target/aarch64-linux-android/release/skyward"))
    for command in ("7za", "avbctl", "resetprop", "unzip", "zstd"):
        name = "system/bin/" + command
        there = find(entries, name)
        if there is not None:
            if there.mode & S_IFMT != S_IFLNK:
                raise SystemExit("%s is not a link" % name)
            there.data = b"recovery"
        else:
            add(entries, name, S_IFLNK | 0o755, b"recovery")
    add(entries, "skyward/system-init", S_IFREG | 0o755, read("target/system-init/aarch64-linux-android/release/system-init"))
    add(entries, "skyward/device.toml", S_IFREG | 0o644, read("devices/warsaw.toml"))
    add(entries, "skyward/MiSansVF.ttf", S_IFREG | 0o644, read("resources/fonts/MiSansVF.ttf"))
    add(entries, "skyward/KirkMono-Regular.ttf", S_IFREG | 0o644, read("resources/fonts/KirkMono-Regular.ttf"))
    add(entries, "skyward/KirkMono-OFL.txt", S_IFREG | 0o644, read("resources/fonts/KirkMono-OFL.txt"))
    if find(entries, "system/etc/task_profiles.json") is None:
        add(entries, "system/etc/task_profiles.json", S_IFREG | 0o644, read("crates/skyward-pack/src/task_profiles.json"))
    add(entries, "system/etc/init/skyward-mtp.rc", S_IFREG | 0o644, read("crates/skyward-pack/src/mtp.rc"))

    props = find(entries, "prop.default")
    if props is None:
        raise SystemExit("prop.default is not in the ramdisk")
    props.data = without(props.data, BUILD_VERSION) + PROPS

    packed = encode_cpio(entries)
    comp = lz4_legacy(packed, lz4)
    print("new ramdisk: %d entries, %d bytes, %d compressed" % (len(entries), len(packed), len(comp)))
    struct.pack_into("<I", header, 12, len(comp))
    image = bytes(header) + comp
    salt = hashlib.sha256(image).digest()
    final = footed(image, "recovery", len(raw), salt)
    open(out, "wb").write(final)
    print("wrote %s (%d bytes)" % (out, len(final)))


def decompress(data):
    if data[:4] == b"\x04\x22\x4d\x18" or data[:4] == b"\x02\x21\x4c\x18":
        with tempfile.NamedTemporaryFile(delete=False, suffix=".lz4") as f:
            f.write(data)
            path = f.name
        try:
            return subprocess.run([LZ4, "-d", "-c", path], check=True, capture_output=True).stdout
        finally:
            os.unlink(path)
    raise SystemExit("the ramdisk is not LZ4")


def read(path):
    return open(path, "rb").read()


if __name__ == "__main__":
    HERE = os.path.dirname(os.path.abspath(__file__))
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        self_test()
    else:
        LZ4 = sys.argv[3]
        main()
