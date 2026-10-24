#!/usr/bin/env python3
import os
import subprocess
from PIL import Image

def generate_icns(source_png, output_icns):
    iconset_dir = "AppIcon.iconset"
    os.makedirs(iconset_dir, exist_ok=True)
    img = Image.open(source_png).convert("RGBA")

    # Standard macOS icon resolutions
    sizes = [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    ]

    for size, filename in sizes:
        resized = img.resize((size, size), Image.Resampling.LANCZOS)
        resized.save(os.path.join(iconset_dir, filename), "PNG")

    subprocess.run(["iconutil", "-c", "icns", iconset_dir, "-o", output_icns], check=True)
    subprocess.run(["rm", "-rf", iconset_dir], check=True)
    print(f"Generated {output_icns} from {source_png}")

if __name__ == "__main__":
    dir_path = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    src = os.path.join(dir_path, "assets", "icon_syncing.png")
    dst = os.path.join(dir_path, "assets", "AppIcon.icns")
    generate_icns(src, dst)
