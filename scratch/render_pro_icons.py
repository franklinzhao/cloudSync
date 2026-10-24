from PIL import Image, ImageDraw, ImageFilter
import math
import numpy as np

SIZE = 512  # Render at 512x512 with supersampling, downscale to 128x128

def draw_cloud_base(draw, fill_color, stroke_color=None, stroke_width=0):
    # Cloud coordinates at 512x512
    # Base pill:
    # left circle: cx=140, cy=330, r=75
    # center big dome: cx=245, cy=240, r=120
    # right dome: cx=370, cy=300, r=90
    # bottom bar connecting them
    
    if stroke_color and stroke_width > 0:
        # Draw stroke first by drawing expanded shapes
        sw = stroke_width
        draw.ellipse([140 - 75 - sw, 330 - 75 - sw, 140 + 75 + sw, 330 + 75 + sw], fill=stroke_color)
        draw.ellipse([245 - 120 - sw, 240 - 120 - sw, 245 + 120 + sw, 240 + 120 + sw], fill=stroke_color)
        draw.ellipse([370 - 90 - sw, 300 - 90 - sw, 370 + 90 + sw, 300 + 90 + sw], fill=stroke_color)
        draw.rectangle([140, 330 - 75 - sw, 370, 330 + 75 + sw], fill=stroke_color)

    # Main fill
    draw.ellipse([140 - 75, 330 - 75, 140 + 75, 330 + 75], fill=fill_color)
    draw.ellipse([245 - 120, 240 - 120, 245 + 120, 240 + 120], fill=fill_color)
    draw.ellipse([370 - 90, 300 - 90, 370 + 90, 300 + 90], fill=fill_color)
    draw.rectangle([140, 255, 370, 405], fill=fill_color)

def generate_idle(filename):
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    
    # Outer dark stroke for light mode visibility (rgba(25, 25, 30, 230))
    draw_cloud_base(draw, fill_color=(255, 255, 255, 255), stroke_color=(20, 20, 25, 220), stroke_width=22)
    
    # Inner subtle highlight / accent line
    draw.arc([145, 140, 345, 340], start=200, end=340, fill=(235, 245, 255, 200), width=10)

    # Downsample with Lanczos to 128x128
    final = img.resize((128, 128), Image.Resampling.LANCZOS)
    final.save(filename, "PNG")
    print(f"Generated {filename}")

def generate_syncing(filename):
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    
    # Base cloud
    draw_cloud_base(draw, fill_color=(255, 255, 255, 255), stroke_color=(20, 20, 25, 220), stroke_width=22)
    
    # Sync Badge at bottom right: cx=370, cy=350, r=115
    cx, cy, r = 370, 350, 115
    
    # Badge dark border
    draw.ellipse([cx - r - 16, cy - r - 16, cx + r + 16, cy + r + 16], fill=(20, 20, 25, 240))
    # Badge vibrant blue circle
    draw.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(0, 122, 255, 255))
    
    # Two thick curved refresh arrows
    arrow_color = (255, 255, 255, 255)
    arrow_r = 65
    aw = 20
    # Top arc
    draw.arc([cx - arrow_r, cy - arrow_r, cx + arrow_r, cy + arrow_r], start=40, end=170, fill=arrow_color, width=aw)
    # Arrowhead 1 at 170 deg
    rad1 = math.radians(170)
    tip1_x = cx + arrow_r * math.cos(rad1)
    tip1_y = cy + arrow_r * math.sin(rad1)
    draw.polygon([
        (tip1_x, tip1_y),
        (tip1_x + 28, tip1_y - 24),
        (tip1_x + 8, tip1_y + 36),
    ], fill=arrow_color)

    # Bottom arc
    draw.arc([cx - arrow_r, cy - arrow_r, cx + arrow_r, cy + arrow_r], start=220, end=350, fill=arrow_color, width=aw)
    # Arrowhead 2 at 350 deg
    rad2 = math.radians(350)
    tip2_x = cx + arrow_r * math.cos(rad2)
    tip2_y = cy + arrow_r * math.sin(rad2)
    draw.polygon([
        (tip2_x, tip2_y),
        (tip2_x - 28, tip2_y + 24),
        (tip2_x - 8, tip2_y - 36),
    ], fill=arrow_color)

    final = img.resize((128, 128), Image.Resampling.LANCZOS)
    final.save(filename, "PNG")
    print(f"Generated {filename}")

def generate_error(filename):
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    
    # Base cloud
    draw_cloud_base(draw, fill_color=(255, 255, 255, 255), stroke_color=(20, 20, 25, 220), stroke_width=22)
    
    # Error Badge at bottom right: cx=370, cy=350, r=115
    cx, cy, r = 370, 350, 115
    
    # Badge dark border
    draw.ellipse([cx - r - 16, cy - r - 16, cx + r + 16, cy + r + 16], fill=(20, 20, 25, 240))
    # Badge vibrant red circle (#FF3B30)
    draw.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(255, 59, 48, 255))
    
    # Exclamation mark in bold white
    # Top bar
    draw.rounded_rectangle([cx - 16, cy - 70, cx + 16, cy + 15], radius=14, fill=(255, 255, 255, 255))
    # Bottom dot
    draw.ellipse([cx - 16, cy + 35, cx + 16, cy + 67], fill=(255, 255, 255, 255))

    final = img.resize((128, 128), Image.Resampling.LANCZOS)
    final.save(filename, "PNG")
    print(f"Generated {filename}")

generate_idle("scratch/test_idle.png")
generate_syncing("scratch/test_syncing.png")
generate_error("scratch/test_error.png")
