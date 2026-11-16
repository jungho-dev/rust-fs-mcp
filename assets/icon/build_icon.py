"""build_icon.py
Build the multi-size Windows icon from the two lossless WebP masters with ImageMagick.
<name>.webp (1024 px tile) feeds 40-256 px, <name>-small.webp (256 px hexagon crop) feeds 16-32 px.

Usage: uv run --no-project python assets/icon/build_icon.py
"""

import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ICON_DIR = Path(__file__).resolve().parent
APP_NAME = "rust-fs-mcp"
LARGE_MASTER = ICON_DIR / f"{APP_NAME}.webp"
SMALL_MASTER = ICON_DIR / f"{APP_NAME}-small.webp"
OUTPUT_ICO = ICON_DIR / f"{APP_NAME}.ico"
# 32 px and below come from the hexagon-only master so the symbol stays legible
SMALL_SIZES = (16, 20, 24, 32)
LARGE_SIZES = (40, 48, 64, 128, 256)


def render_png(magick: str, master: Path, size: int, out_png: Path) -> bytes:
    """Render one Lanczos-scaled 32-bit PNG entry from a master image."""
    subprocess.run(
        [magick, str(master), "-filter", "Lanczos", "-resize", f"{size}x{size}",
         "-depth", "8", f"PNG32:{out_png}"],
        check=True,
    )
    return out_png.read_bytes()


def build_icon(magick: str) -> Path:
    """Pack ICONDIR + ICONDIRENTRY records + PNG bodies (PNG entries, Vista and later)."""
    with tempfile.TemporaryDirectory(prefix="icon-") as work_text:
        work_dir = Path(work_text)
        png_by_size: dict[int, bytes] = {}
        for master, sizes in ((SMALL_MASTER, SMALL_SIZES), (LARGE_MASTER, LARGE_SIZES)):
            for size in sizes:
                png_by_size[size] = render_png(magick, master, size, work_dir / f"{size}.png")

        sizes = sorted(png_by_size)
        header = struct.pack("<HHH", 0, 1, len(sizes))
        entries = b""
        bodies = b""
        offset = 6 + 16 * len(sizes)
        for size in sizes:
            body = png_by_size[size]
            dim = 0 if size >= 256 else size
            entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(body), offset)
            bodies += body
            offset += len(body)
        OUTPUT_ICO.write_bytes(header + entries + bodies)
    return OUTPUT_ICO


if __name__ == "__main__":
    magick_path = shutil.which("magick")
    if magick_path is None:
        sys.exit("ImageMagick (magick) was not found on PATH")
    ico_path = build_icon(magick_path)
    print(f"{ico_path} ({ico_path.stat().st_size} bytes)")
