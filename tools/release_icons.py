"""Original vector-style app icon, rendered without external images or metadata."""
from pathlib import Path
import struct
import zlib

def png(size):
    shapes = [
        (0.035,0.035,0.965,0.965,0.21,(36,45,64)),
        (0.15,0.15,0.53,0.85,0.035,(71,143,247)),
        (0.57,0.15,0.85,0.48,0.035,(83,187,167)),
        (0.57,0.52,0.85,0.85,0.035,(244,180,84)),
    ]
    data = bytearray()
    for y in range(size):
        data.append(0)
        fy=(y+0.5)/size
        for x in range(size):
            fx=(x+0.5)/size
            pixel=(0,0,0,0)
            for left,top,right,bottom,radius,color in shapes:
                if not (left <= fx <= right and top <= fy <= bottom):
                    continue
                cx=min(max(fx,left+radius),right-radius)
                cy=min(max(fy,top+radius),bottom-radius)
                if (fx-cx)**2+(fy-cy)**2 <= radius**2:
                    pixel=(*color,255)
            data.extend(pixel)
    def chunk(kind, content):
        return struct.pack(">I",len(content))+kind+content+struct.pack(">I",zlib.crc32(kind+content)&0xffffffff)
    return b"\x89PNG\r\n\x1a\n"+chunk(b"IHDR",struct.pack(">IIBBBBB",size,size,8,6,0,0,0))+chunk(b"IDAT",zlib.compress(data,9))+chunk(b"IEND",b"")

def iconset(path):
    path=Path(path);path.mkdir(parents=True,exist_ok=True)
    cache={}
    for base in [16,32,128,256,512]:
        for factor in [1,2]:
            size=base*factor
            if size not in cache:cache[size]=png(size)
            suffix="@2x" if factor==2 else ""
            (path/f"icon_{base}x{base}{suffix}.png").write_bytes(cache[size])


def ico():
    sizes = [16, 24, 32, 48, 64, 128, 256]
    images = [png(size) for size in sizes]
    offset = 6 + 16 * len(images)
    result = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    for size, data in zip(sizes, images):
        result.extend(struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset))
        offset += len(data)
    return bytes(result) + b"".join(images)


def desktop_assets(path):
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    (path / "WizTree.png").write_bytes(png(256))
    (path / "WizTree.ico").write_bytes(ico())


if __name__ == "__main__":
    desktop_assets(Path(__file__).resolve().parents[1] / "crates/desktop/assets")
