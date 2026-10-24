from PIL import Image, ImageDraw, ImageFilter
import math

SIZE = 512  # Render at 512x512, then downscale to 128x128 for pristine anti-aliasing

def create_cloud_mask(size=512):
    # Create cloud shape mask
    mask = Image.new("L", (size, size), 0)
    draw = ImageDraw.Draw(mask)
    
    # Cloud geometry scaled for 512x512
    # Bounding box roughly: x from 40 to 472, y from 130 to 390 (height 260, width 432)
    # Centers and radii for cloud puffs:
    # Bottom rounded rectangle:
    draw.rounded_rectangle([60, 240, 452, 380], radius=70, fill=255)
    
    # Left puff:
    draw.ellipse([60, 210, 220, 370], fill=255)
    # Middle/top big puff:
    draw.ellipse([160, 120, 360, 320], fill=255)
    # Right puff:
    draw.ellipse([300, 190, 452, 350], fill=255)
    
    return mask

def draw_sync_arrows(draw, cx, cy, r, color=(255, 255, 255, 255), width=18):
    # Two curved arcs forming sync circle
    # Arc 1: 30 deg to 150 deg
    bbox = [cx - r, cy - r, cx + r, cy + r]
    draw.arc(bbox, start=25, end=155, fill=color, width=width)
    # Arrow head for arc 1 at end (155 deg)
    rad = math.radians(155)
    tip_x = cx + r * math.cos(rad)
    tip_y = cy + r * math.sin(rad)
    draw.polygon([
        (tip_x, tip_y),
        (tip_x + 22, tip_y - 20),
        (tip_x + 5, tip_y + 28),
    ], fill=color)

    # Arc 2: 205 deg to 335 deg
    draw.arc(bbox, start=205, end=335, fill=color, width=width)
    # Arrow head for arc 2 at end (335 deg)
    rad2 = math.radians(335)
    tip2_x = cx + r * math.cos(rad2)
    tip2_y = cy + r * math.sin(rad2)
    draw.polygon([
        (tip2_x, tip2_y),
        (tip2_x - 22, tip2_y + 20),
        (tip2_x - 5, tip2_y - 28),
    ], fill=color)

print("Script template ready")
