"""Regenerates the WiX dialog artwork from assets/branding (requires Pillow).

    python installer/make-art.py

WixUI draws its text on the white areas, so keep them white: the welcome and
finish dialogs put text to the right of the left panel (about x >= 180 px), and
every other dialog draws its title in the left part of the banner.
"""

from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ART = ROOT / "assets" / "branding"
OUT = ROOT / "installer"
CHARCOAL = (38, 38, 38)
ORANGE = (254, 112, 10)
WHITE = (255, 255, 255)


def fit(path: Path, width: int, height: int) -> Image.Image:
    im = Image.open(path).convert("RGBA")
    im.thumbnail((width, height), Image.LANCZOS)
    return im


def dialog() -> Image.Image:
    # 493 x 312: the left panel sits behind the welcome/finish dialogs' artwork column.
    panel_w = 164
    img = Image.new("RGBA", (493, 312), WHITE + (255,))
    img.paste(Image.new("RGBA", (panel_w, 312), CHARCOAL + (255,)), (0, 0))
    img.paste(Image.new("RGBA", (3, 312), ORANGE + (255,)), (panel_w - 3, 0))
    mark = fit(ART / "mark-dark.png", 120, 120)
    word = fit(ART / "wordmark-dark.png", 112, 40)
    top = 96
    img.alpha_composite(mark, ((panel_w - 3 - mark.width) // 2, top))
    img.alpha_composite(word, ((panel_w - 3 - word.width) // 2, top + mark.height + 14))
    return img


def banner() -> Image.Image:
    # 493 x 58: dialog titles are drawn over the left of the banner.
    img = Image.new("RGBA", (493, 58), WHITE + (255,))
    mark = fit(ART / "mark-light.png", 84, 40)
    img.alpha_composite(mark, (493 - mark.width - 14, (58 - mark.height) // 2))
    return img


for name, image in [("dialog.bmp", dialog()), ("banner.bmp", banner())]:
    image.convert("RGB").save(OUT / name)
    print(OUT / name)
