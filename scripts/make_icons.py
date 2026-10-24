#!/usr/bin/env python3
"""MCode_Desktop 品牌资产生成：icon（icns/ico/png）绘制与导出。
设计：暗底圆角方形 + 橙色 M 笔画 + 终端块状光标（M ▪，MCode 的 M + 终端提示符）。
4x 超采样抗锯齿；PIL 绘制，无外部图像依赖。"""
from PIL import Image, ImageDraw
import subprocess, sys, os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "assets", "brand")
S = 4  # 超采样倍率

BG_TOP, BG_BOT = (35, 35, 39), (16, 16, 19)
ACCENT = (217, 119, 87)        # 与应用 theme.rs ACCENT 一致
ACCENT_HI = (229, 138, 102)    # 顶部微亮

def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))

def draw_icon(size: int) -> Image.Image:
    n = size * S
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    # 背景：垂直渐变圆角方形
    bg = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    bd = ImageDraw.Draw(bg)
    r = int(n * 0.2265)  # macOS squircle 比例
    for y in range(n):
        bd.line([(0, y), (n, y)], fill=lerp(BG_TOP, BG_BOT, y / n))
    mask = Image.new("L", (n, n), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, n - 1, n - 1], radius=r, fill=255)
    img.paste(bg, (0, 0), mask)
    # 内侧高光描边
    d.rounded_rectangle([int(n*0.008), int(n*0.008), n - int(n*0.008) - 1, n - int(n*0.008) - 1],
                        radius=int(r * 0.99), outline=(255, 255, 255, 18), width=max(1, int(n * 0.004)))
    # M 笔画（顶点，比例坐标 → 1024 基准）
    pts = [(280, 706), (280, 330), (482, 584), (684, 330), (684, 706)]
    scale = n / 1024.0
    P = [(int(x * scale), int(y * scale)) for x, y in pts]
    w = int(78 * scale)
    d.line(P, fill=ACCENT, width=w, joint="curve")
    rad = w // 2
    for (x, y) in P:  # 圆头端点/顶点
        d.ellipse([x - rad, y - rad, x + rad, y + rad], fill=ACCENT)
    # 顶部高光：左竖笔上半段叠亮色（模拟光照）
    hi = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    hd = ImageDraw.Draw(hi)
    hd.line([P[0], P[1]], fill=ACCENT_HI + (255,), width=w, joint="curve")
    for (x, y) in (P[0], P[1]):
        hd.ellipse([x - rad, y - rad, x + rad, y + rad], fill=ACCENT_HI + (255,))
    hi_mask = Image.new("L", (n, n), 0)
    ImageDraw.Draw(hi_mask).line([P[0], P[1]], fill=140, width=w)
    for (x, y) in (P[0], P[1]):
        ImageDraw.Draw(hi_mask).ellipse([x - rad, y - rad, x + rad, y + rad], fill=140)
    img.paste(Image.composite(hi, img, hi_mask), (0, 0))
    # 终端块状光标
    cx0, cy0, cx1, cy1 = [int(v * scale) for v in (748, 628, 814, 706)]
    d.rounded_rectangle([cx0, cy0, cx1, cy1], radius=int(14 * scale), fill=ACCENT)
    return img.resize((size, size), Image.LANCZOS)

def main():
    os.makedirs(OUT, exist_ok=True)
    master = draw_icon(1024)
    master.save(os.path.join(OUT, "icon_1024.png"))
    # macOS iconset → icns
    iconset = os.path.join(OUT, "icon.iconset")
    os.makedirs(iconset, exist_ok=True)
    for sz in (16, 32, 128, 256, 512):
        draw_icon(sz).save(os.path.join(iconset, f"icon_{sz}x{sz}.png"))
        draw_icon(sz * 2).save(os.path.join(iconset, f"icon_{sz}x{sz}@2x.png"))
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(OUT, "icon.icns")], check=True)
    # Windows 多分辨率 ico
    master.save(os.path.join(OUT, "icon.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    print("OK: assets/brand/{icon.icns,icon.ico,icon_1024.png} + iconset")

if __name__ == "__main__":
    main()
