from PIL import Image, ImageDraw, ImageFont

# Create a preview image comparing old assets vs new assets on both dark & light backgrounds
preview = Image.new("RGBA", (800, 420), (30, 32, 36, 255))
draw = ImageDraw.Draw(preview)

# Left half: Light background (240, 240, 242)
draw.rectangle([20, 20, 390, 400], fill=(235, 235, 238, 255), outline=(200, 200, 205, 255), width=2)
# Right half: Dark background (45, 45, 48)
draw.rectangle([410, 20, 780, 400], fill=(42, 44, 48, 255), outline=(65, 68, 75, 255), width=2)

draw.text((40, 35), "LIGHT MENU BAR (macOS Light)", fill=(20, 20, 20, 255))
draw.text((430, 35), "DARK MENU BAR (macOS Dark)", fill=(240, 240, 240, 255))

draw.text((40, 70), "OLD ASSETS (Nearly Invisible):", fill=(80, 80, 80, 255))
draw.text((430, 70), "OLD ASSETS (Thin/Low Contrast):", fill=(180, 180, 180, 255))

old_idle = Image.open("assets/icon_idle.png").resize((44, 44), Image.Resampling.LANCZOS)
old_sync = Image.open("assets/icon_syncing.png").resize((44, 44), Image.Resampling.LANCZOS)
old_err  = Image.open("assets/icon_error.png").resize((44, 44), Image.Resampling.LANCZOS)

# Paste old on light
preview.alpha_composite(old_idle, (50, 105))
preview.alpha_composite(old_sync, (150, 105))
preview.alpha_composite(old_err,  (250, 105))

# Paste old on dark
preview.alpha_composite(old_idle, (440, 105))
preview.alpha_composite(old_sync, (540, 105))
preview.alpha_composite(old_err,  (640, 105))

draw.text((40, 190), "NEW BOLD ASSETS (High Visibility):", fill=(20, 100, 20, 255))
draw.text((430, 190), "NEW BOLD ASSETS (High Visibility):", fill=(100, 220, 120, 255))

new_idle = Image.open("scratch/test_idle.png").resize((44, 44), Image.Resampling.LANCZOS)
new_sync = Image.open("scratch/test_syncing.png").resize((44, 44), Image.Resampling.LANCZOS)
new_err  = Image.open("scratch/test_error.png").resize((44, 44), Image.Resampling.LANCZOS)

# Paste new on light
preview.alpha_composite(new_idle, (50, 230))
preview.alpha_composite(new_sync, (150, 230))
preview.alpha_composite(new_err,  (250, 230))

# Paste new on dark
preview.alpha_composite(new_idle, (440, 230))
preview.alpha_composite(new_sync, (540, 230))
preview.alpha_composite(new_err,  (640, 230))

# Also show 128px full size version
new_idle_large = Image.open("scratch/test_idle.png").resize((70, 70), Image.Resampling.LANCZOS)
new_sync_large = Image.open("scratch/test_syncing.png").resize((70, 70), Image.Resampling.LANCZOS)
new_err_large  = Image.open("scratch/test_error.png").resize((70, 70), Image.Resampling.LANCZOS)

preview.alpha_composite(new_idle_large, (50, 305))
preview.alpha_composite(new_sync_large, (150, 305))
preview.alpha_composite(new_err_large,  (250, 305))

preview.alpha_composite(new_idle_large, (440, 305))
preview.alpha_composite(new_sync_large, (540, 305))
preview.alpha_composite(new_err_large,  (640, 305))

preview.save("scratch/icon_comparison_preview.png")
print("Saved scratch/icon_comparison_preview.png")
