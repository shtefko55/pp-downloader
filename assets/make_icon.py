# Рисува иконата на приложението без външни библиотеки.
#   python assets/make_icon.py
# Създава assets/icon.ico (за exe-то) и assets/icon_64.rgba (за прозореца, сурови RGBA пиксели 64×64).
import math, os, struct, zlib

HERE = os.path.dirname(os.path.abspath(__file__))
SS = 4  # суперсемплиране за гладки ръбове


def lerp(a, b, t):
    return a + (b - a) * t


def mix(c1, c2, t):
    return tuple(lerp(a, b, t) for a, b in zip(c1, c2))


def sd_round_rect(x, y, cx, cy, hw, hh, r):
    qx, qy = abs(x - cx) - hw + r, abs(y - cy) - hh + r
    return math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - r


def sd_tri(px, py, a, b, c):
    # Знаково разстояние до триъгълник (Inigo Quilez).
    def sub(p, q): return (p[0] - q[0], p[1] - q[1])
    def dot(p, q): return p[0] * q[0] + p[1] * q[1]
    p = (px, py)
    e0, e1, e2 = sub(b, a), sub(c, b), sub(a, c)
    v0, v1, v2 = sub(p, a), sub(p, b), sub(p, c)
    def seg(v, e):
        h = max(0.0, min(1.0, dot(v, e) / dot(e, e)))
        return (v[0] - e[0] * h, v[1] - e[1] * h)
    pq0, pq1, pq2 = seg(v0, e0), seg(v1, e1), seg(v2, e2)
    s = 1 if e0[0] * e2[1] - e0[1] * e2[0] > 0 else -1
    d = min((dot(pq0, pq0), s * (v0[0] * e0[1] - v0[1] * e0[0])),
            (dot(pq1, pq1), s * (v1[0] * e1[1] - v1[1] * e1[0])),
            (dot(pq2, pq2), s * (v2[0] * e2[1] - v2[1] * e2[0])))
    return -math.sqrt(d[0]) * (1 if d[1] > 0 else -1)


BLUE = (40, 110, 200)
PURPLE = (125, 70, 185)
AMBER = (245, 165, 40)
WHITE = (255, 255, 255)


def sample(x, y):
    """x, y в 0..1. Връща (r, g, b, a) с a в 0..1."""
    layers = []
    # Фон: заоблен квадрат с диагонален градиент синьо → лилаво.
    d_bg = sd_round_rect(x, y, 0.5, 0.5, 0.46, 0.46, 0.2)
    if d_bg <= 0:
        t = (x + y) / 2
        col = mix(BLUE, PURPLE, t)
        # Лек отблясък горе.
        glow = max(0.0, 1 - math.hypot(x - 0.3, y - 0.15) / 0.6) * 0.18
        col = mix(col, WHITE, glow)
        layers.append((col, 1.0))
    else:
        return (0, 0, 0, 0.0)
    # Кадър от филмова лента: тъмна заоблена рамка с перфорации.
    d_film = sd_round_rect(x, y, 0.44, 0.44, 0.27, 0.22, 0.05)
    if d_film <= 0:
        layers.append(((20, 22, 40), 0.35))
        # перфорации горе и долу
        for row in (0.255, 0.625):
            for i in range(6):
                hx = 0.215 + i * 0.09
                if sd_round_rect(x, y, hx, row, 0.022, 0.016, 0.008) <= 0:
                    layers.append((WHITE, 0.55))
    # Бял триъгълник „play“.
    d_play = sd_tri(x, y, (0.37, 0.335), (0.37, 0.545), (0.555, 0.44)) - 0.018
    if d_play <= 0:
        layers.append((WHITE, 1.0))
    # Значка долу вдясно: бял пръстен + кехлибарен кръг със стрелка надолу.
    cx, cy, r = 0.72, 0.72, 0.17
    dc = math.hypot(x - cx, y - cy)
    if dc <= r + 0.03:
        layers.append((WHITE, 1.0))
    if dc <= r:
        layers.append((mix(AMBER, (230, 120, 30), (y - cy + r) / (2 * r)), 1.0))
        stem = sd_round_rect(x, y, cx, cy - 0.035, 0.024, 0.065, 0.012)
        head = sd_tri(x, y, (cx - 0.085, cy + 0.0), (cx + 0.085, cy + 0.0), (cx, cy + 0.095)) - 0.01
        base = sd_round_rect(x, y, cx, cy + 0.125, 0.085, 0.014, 0.012)
        if min(stem, head) <= 0 or base <= 0:
            layers.append((WHITE, 1.0))
    col = (0.0, 0.0, 0.0)
    for c, a in layers:
        col = mix(col, c, a)
    return (*col, 1.0)


def render(size):
    px = bytearray()
    n = SS * SS
    for j in range(size):
        for i in range(size):
            r = g = b = a = 0.0
            for sj in range(SS):
                for si in range(SS):
                    x = (i + (si + 0.5) / SS) / size
                    y = (j + (sj + 0.5) / SS) / size
                    cr, cg, cb, ca = sample(x, y)
                    r += cr * ca; g += cg * ca; b += cb * ca; a += ca
            if a > 0:
                px += bytes((round(r / a), round(g / a), round(b / a), round(255 * a / n)))
            else:
                px += b"\0\0\0\0"
    return bytes(px)


def png(size, rgba):
    raw = b"".join(b"\0" + rgba[y * size * 4:(y + 1) * size * 4] for y in range(size))
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def main():
    sizes = [16, 24, 32, 48, 64, 128, 256]
    images = {s: render(s) for s in sizes}
    pngs = [png(s, images[s]) for s in sizes]
    head = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries = b""
    for s, data in zip(sizes, pngs):
        entries += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    with open(os.path.join(HERE, "icon.ico"), "wb") as f:
        f.write(head + entries + b"".join(pngs))
    with open(os.path.join(HERE, "icon_64.rgba"), "wb") as f:
        f.write(images[64])
    with open(os.path.join(HERE, "icon_256.png"), "wb") as f:
        f.write(pngs[-1])


if __name__ == "__main__":
    main()
