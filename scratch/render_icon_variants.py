from PIL import Image, ImageDraw, ImageFilter
import math

SIZE = 512

def draw_cloud(draw, fill_color, stroke_color, sw=24, interior_line=True):
    # Proportions:
    # Left puff: cx=145, cy=325, r=75
    # Center dome: cx=255, cy=225, r=125
    # Right puff: cx=375, cy=305, r=90
    # Bottom rect: [145, 325, 375, 400]
    
    # 1. Outer stroke
    if stroke_color and sw > 0:
        draw.ellipse([145 - 75 - sw, 325 - 75 - sw, 145 + 75 + sw, 325 + 75 + sw], fill=stroke_color)
        draw.ellipse([255 - 125 - sw, 225 - 125 - sw, 255 + 125 + sw, 225 + 125 + sw], fill=stroke_color)
        draw.ellipse([375 - 90 - sw, 305 - 90 - sw, 375 + 90 + sw, 305 + 90 + sw], fill=stroke_color)
        draw.rectangle([145, 325 - 75 - sw, 375, 400 + sw], fill=stroke_color)
        draw.rounded_rectangle([145 - 75 - sw, 400 - sw, 375 + 90 + sw, 400 + sw], radius=sw, fill=stroke_color)

    # 2. Main fill
    draw.ellipse([145 - 75, 325 - 75, 145 + 75, 325 + 75], fill=fill_color)
    draw.ellipse([255 - 125, 225 - 125, 255 + 125, 225 + 125], fill=fill_color)
    draw.ellipse([375 - 90, 305 - 90, 375 + 90, 305 + 90], fill=fill_color)
    draw.rectangle([145, 250, 375, 400], fill=fill_color)
    draw.rounded_rectangle([145 - 75, 325, 375 + 90, 400], radius=38, fill=fill_color)

    # 3. Interior subtle depth arc
    if interior_line:
        # Arc between center dome and right puff
        draw.arc([130, 250, 270, 390], start=290, end=360, fill=(215, 225, 238, 220), width=12)
        draw.arc([280, 220, 420, 360], start=180, end=270, fill=(215, 225, 238, 220), width=12)

def draw_sync_badge(draw, cx=375, cy=355, r=115):
    # Badge border
    draw.ellipse([cx - r - 18, cy - r - 18, cx + r + 18, cy + r + 18], fill=(15, 18, 24, 250))
    # Badge background - Vibrant Electric Blue
    draw.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(0, 122, 255, 255))
    
    # 2 Curved sync arrows in pure white
    arrow_r = 65
    aw = 20
    # Top arc
    draw.arc([cx - arrow_r, cy - arrow_r, cx + arrow_r, cy + arrow_r], start=35, end=175, fill=(255, 255, 255, 255), width=aw)
    # Head 1
    rad1 = math.radians(175)
    t1x = cx + arrow_r * math.cos(rad1)
    t1y = cy + arrow_r * math.sin(rad1)
    draw.polygon([(t1x, t1y), (t1x + 30, t1y - 26), (t1x + 8, t1y + 36)], fill=(255, 255, 255, 255))

    # Bottom arc
    draw.arc([cx - arrow_r, cy - arrow_r, cx + arrow_r, cy + arrow_r], start=215, end=355, fill=(255, 255, 255, 255), width=aw)
    # Head 2
    rad2 = math.radians(355)
    t2x = cx + arrow_r * math.cos(rad2)
    t2y = cy + arrow_r * math.sin(rad2)
    draw.polygon([(t2x, t2y), (t2x - 30, t2y + 26), (t2x - 8, t2y - 36)], fill=(255, 255, 255, 255))

def draw_error_badge(draw, cx=375, cy=355, r=115):
    # Badge border
    draw.ellipse([cx - r - 18, cy - r - 18, cx + r + 18, cy + r + 18], fill=(15, 18, 24, 250))
    # Badge background - Vivid Apple Red
    draw.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(255, 59, 48, 255))
    
    # Exclamation mark
    draw.rounded_rectangle([cx - 18, cy - 70, cx + 18, cy + 18], radius=15, fill=(255, 255, 255, 255))
    draw.ellipse([cx - 18, cy + 38, cx + 18, cy + 74], fill=(255, 255, 255, 255))

for name, has_badge in [("idle", None), ("syncing", "sync"), ("error", "error")]:
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw_cloud(draw, fill_color=(255, 255, 255, 255), stroke_color=(25, 28, 35, 230), sw=22, interior_line=(has_badge is None))
    if has_badge == "sync":
        draw_sync_badge(draw)
    elif has_badge == "error":
        draw_error_badge(draw)
    
    # Save 128x128
    res128 = img.resize((128, 128), Image.Resampling.LANCZOS)
    res128.save(f"scratch/final_{name}.png")

print("Generated scratch/final_idle.png, final_syncing.png, final_error.png")
