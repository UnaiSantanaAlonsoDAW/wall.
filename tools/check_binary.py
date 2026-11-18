"""Optional packaging check; uses only Python's standard library."""
from pathlib import Path
import hashlib
import struct
import sys
import zipfile
import tomllib

path = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/wall.exe")
data = path.read_bytes()
pe = struct.unpack_from("<I", data, 0x3C)[0]
assert data[pe:pe + 4] == b"PE\0\0", "Not a Windows executable"
assert struct.unpack_from("<H", data, pe + 4)[0] == 0x8664, "Not x64"
optional = pe + 24
assert struct.unpack_from("<H", data, optional + 68)[0] == 2, "Not a GUI executable"
count = struct.unpack_from("<H", data, pe + 6)[0]
headers = optional + struct.unpack_from("<H", data, pe + 20)[0]
sections = [struct.unpack_from("<IIII", data, headers + i * 40 + 8) for i in range(count)]


def offset(rva):
    for size, address, raw_size, raw in sections:
        if address <= rva < address + max(size, raw_size):
            return raw + rva - address
    raise ValueError(f"Unmapped PE address: {rva:x}")


cursor = offset(struct.unpack_from("<I", data, optional + 112 + 8)[0])
imports = []
while any(data[cursor:cursor + 20]):
    name = offset(struct.unpack_from("<I", data, cursor + 12)[0])
    imports.append(data[name:data.index(0, name)].decode())
    cursor += 20
assert not any(name.lower().startswith(("vcruntime", "msvcp140")) for name in imports), "External VC redistributable required"
print(f"PASS: x64 GUI executable, {len(data):,} bytes; no external VCRuntime dependency")
print("Imports: " + ", ".join(imports))
print("SHA256: " + hashlib.sha256(data).hexdigest())

resource_rva = struct.unpack_from("<I", data, optional + 112 + 2 * 8)[0]
assert resource_rva, "Missing Windows resources"
resources = offset(resource_rva)


def directory(relative):
    table = resources + (relative & 0x7FFF_FFFF)
    named, numbered = struct.unpack_from("<HH", data, table + 12)
    return dict(
        struct.unpack_from("<II", data, table + 16 + index * 8)
        for index in range(named, named + numbered)
    )


def resource(kind, number):
    languages = directory(directory(directory(0)[kind])[number])
    entry = resources + next(iter(languages.values()))
    rva, length = struct.unpack_from("<II", data, entry)
    start = offset(rva)
    return data[start:start + length]


expected_sizes = {(size, size) for size in (16, 24, 32, 48, 256)}
for number, filename in ((101, "wall.ico"), (102, "tray-light.ico"), (103, "tray-dark.ico")):
    group = resource(14, number)  # RT_GROUP_ICON
    reserved, kind, count = struct.unpack_from("<HHH", group)
    assert (reserved, kind, count) == (0, 1, len(expected_sizes)), f"Bad icon group {number}"
    source = (Path(__file__).resolve().parents[1] / "assets" / filename).read_bytes()
    frames = {}
    for index in range(count):
        w, h, _, _, _, _, size, start = struct.unpack_from("<BBBBHHII", source, 6 + index * 16)
        frames[(w or 256, h or 256)] = source[start:start + size]
    sizes = set()
    for index in range(count):
        w, h, _, _, _, bits, size, icon = struct.unpack_from("<BBBBHHIH", group, 6 + index * 14)
        dimensions = (w or 256, h or 256)
        sizes.add(dimensions)
        frame = resource(3, icon)  # RT_ICON
        assert bits == 32 and len(frame) == size and frame == frames[dimensions], (
            f"Embedded {filename} frame differs at {dimensions}"
        )
    assert sizes == expected_sizes, f"Missing sizes in {filename}"
print("PASS: executable and light/dark tray icons embed all five approved raster sizes")

version = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
archive = Path(f"dist/wall-{version}-windows-x64.zip")
delivered = Path("dist/wall.exe")
if "--binary-only" not in sys.argv and archive.exists() and delivered.exists():
    assert delivered.read_bytes() == data, "Delivered binary differs"
    with zipfile.ZipFile(archive) as package:
        assert package.read("wall.exe") == data, "ZIP binary differs"
        assert {"README.md", "README.es.md", "LICENSE", "VALIDATION.md", "THIRD_PARTY_NOTICES.md"} <= set(package.namelist())
    print("PASS: delivered executable and ZIP contain the same verified binary and documentation")
