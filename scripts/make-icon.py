"""Generates the Chud Clone icon: two twin celestial bodies linked by a stream of light."""
import math, sys
from PIL import Image, ImageDraw, ImageFilter

S = 1024
def lerp(a, b, t): return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))
CYAN, ELEC, VIOLET, VOID = (79, 227, 255), (58, 123, 255), (154, 107, 255), (5, 6, 13)

img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
# Background: rounded deep-night square with a purple halo.
bg = Image.new("RGBA", (S, S), VOID + (255,))
glow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
g = ImageDraw.Draw(glow)
g.ellipse((S * 0.35, -S * 0.25, S * 1.25, S * 0.65), fill=(60, 30, 130, 150))
g.ellipse((-S * 0.3, S * 0.5, S * 0.5, S * 1.3), fill=(15, 50, 120, 140))
bg = Image.alpha_composite(bg, glow.filter(ImageFilter.GaussianBlur(S * 0.12)))
mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle((0, 0, S - 1, S - 1), radius=int(S * 0.22), fill=255)
img.paste(bg, (0, 0), mask)
d = ImageDraw.Draw(img)

def planet(cx, cy, r, c1, c2, alpha=255):
    layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ld = ImageDraw.Draw(layer)
    for i in range(int(r), 0, -2):
        t = i / r
        ld.ellipse((cx - i, cy - i, cx + i, cy + i), fill=lerp(c1, c2, t) + (alpha,))
    # Drop shadow for depth.
    sh = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(sh).ellipse((cx - r * 0.55, cy - r * 0.75, cx + r * 1.45, cy + r * 1.25), fill=(5, 6, 13, 150))
    pm = Image.new("L", (S, S), 0)
    ImageDraw.Draw(pm).ellipse((cx - r, cy - r, cx + r, cy + r), fill=255)
    layer.paste(Image.alpha_composite(layer, sh.filter(ImageFilter.GaussianBlur(r * 0.25))), (0, 0), pm)
    return layer

# Halo of the celestial bodies.
halo = Image.new("RGBA", (S, S), (0, 0, 0, 0))
hd = ImageDraw.Draw(halo)
hd.ellipse((150, 290, 510, 650), fill=CYAN + (110,))
hd.ellipse((560, 420, 880, 740), fill=VIOLET + (110,))
img = Image.alpha_composite(img, halo.filter(ImageFilter.GaussianBlur(60)))

# Flow between the two bodies: dotted arc, cyan → purple.
flow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
fd = ImageDraw.Draw(flow)
for i in range(60):
    t = i / 59
    x = 330 + (720 - 330) * t
    y = 470 + (580 - 470) * t - math.sin(t * math.pi) * 230
    rad = 9 + 13 * math.sin(t * math.pi)
    fd.ellipse((x - rad, y - rad, x + rad, y + rad), fill=lerp(CYAN, VIOLET, t) + (235,))
img = Image.alpha_composite(img, flow.filter(ImageFilter.GaussianBlur(14)))
img = Image.alpha_composite(img, flow)

img = Image.alpha_composite(img, planet(330, 470, 170, (190, 250, 255), ELEC))
img = Image.alpha_composite(img, planet(720, 580, 150, (225, 205, 255), (90, 60, 200)))
# Thin ring around the clone: "true copy".
ring = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(ring).ellipse((720 - 190, 580 - 190, 720 + 190, 580 + 190), outline=CYAN + (200,), width=10)
img = Image.alpha_composite(img, ring)

out = sys.argv[1]
img.save(f"{out}/icon.png")
for n in (32, 128, 256):
    img.resize((n, n), Image.LANCZOS).save(f"{out}/{n}x{n}.png")
img.resize((256, 256), Image.LANCZOS).save(f"{out}/128x128@2x.png")
img.save(f"{out}/icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
print("icons written to", out)
