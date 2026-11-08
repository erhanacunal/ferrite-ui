#!/usr/bin/env python3
"""Writes knot.obj: a trefoil knot as a tube of quads with vertex normals, the
example's OBJ mesh ("meshes" in project.json). Run it again after changing the
sizes below; ferrite_build converts the OBJ into a RES_MESH resource.

usage: python make_knot.py [OUT.obj]
"""

import math
import sys

SEGMENTS = 160   # along the knot
SIDES = 12       # around the tube
TUBE = 0.42      # tube radius (the knot is about 3 across; ferrite_build rescales)


def curve(t):
    """The trefoil knot."""
    return (math.sin(t) + 2 * math.sin(2 * t), math.cos(t) - 2 * math.cos(2 * t), -math.sin(3 * t))


def sub(a, b):
    return tuple(x - y for x, y in zip(a, b))


def cross(a, b):
    return (a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0])


def unit(v):
    n = math.sqrt(sum(x * x for x in v))
    return tuple(x / n for x in v)


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "knot.obj"
    step = 2 * math.pi / SEGMENTS
    points = [curve(i * step) for i in range(SEGMENTS)]
    tangents = [unit(sub(points[(i + 1) % SEGMENTS], points[i - 1])) for i in range(SEGMENTS)]
    # A frame carried along the curve (parallel transport), then twisted back
    # evenly so that the tube's seam closes.
    normal = unit(cross(tangents[0], (0.0, 0.0, 1.0)))
    frames = []
    for t in tangents:
        normal = unit(cross(cross(t, normal), t))
        frames.append((normal, cross(t, normal)))
    n0, b0 = frames[0]
    n_end = unit(cross(cross(tangents[0], frames[-1][0]), tangents[0]))
    twist = math.atan2(sum(a * b for a, b in zip(n_end, b0)), sum(a * b for a, b in zip(n_end, n0)))

    lines = ["# Trefoil knot tube, written by make_knot.py"]
    normals = []
    for i, (p, (n, b)) in enumerate(zip(points, frames)):
        back = -twist * i / SEGMENTS
        for j in range(SIDES):
            a = 2 * math.pi * j / SIDES + back
            d = tuple(math.cos(a) * x + math.sin(a) * y for x, y in zip(n, b))
            lines.append("v %.4f %.4f %.4f" % tuple(c + TUBE * e for c, e in zip(p, d)))
            normals.append("vn %.4f %.4f %.4f" % d)
    lines += normals
    for i in range(SEGMENTS):
        for j in range(SIDES):
            q = [((i + di) % SEGMENTS) * SIDES + (j + dj) % SIDES + 1 for di, dj in ((0, 0), (1, 0), (1, 1), (0, 1))]
            lines.append("f " + " ".join(f"{k}//{k}" for k in q))
    with open(out, "w", encoding="ascii", newline="\n") as f:
        f.write("\n".join(lines) + "\n")
    print(f"{out}: {SEGMENTS * SIDES} vertices, {SEGMENTS * SIDES * 2} triangles")


if __name__ == "__main__":
    main()
