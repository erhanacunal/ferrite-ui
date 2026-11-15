"""Rich label text (caps.rich_text): the small HTML subset ferrite-ui-xl's
ferrite-core `rich.rs` reads. The Designer's preview uses it; keep the two in
sync.

- <b>/<strong>, <i>/<em>, <u>: bold, italic, underline (nest; each close tag
  ends one open). Bold / italic draw with the label font's variants (project
  font entries' "bold" / "italic" / "bold_italic").
- <font color="..."> ... </font>: #RRGGBB, #RGB, a colour name, or an RGB565
  number (63488, 0xF800). Quotes optional. 8 levels kept.
- <br> / <br/>: line break. Entities: &lt; &gt; &amp; &quot; &apos; &nbsp;
  &#NNN; &#xHHHH;. Other tags are left out; a '<' or '&' that doesn't start
  one is text.
"""

BOLD, ITALIC, UNDERLINE = 1, 2, 4
COLOR_DEPTH = 8
TAG_MAX = 64


def _rgb(r, g, b):
    return (r >> 3) << 11 | (g >> 2) << 5 | b >> 3


COLOR_NAMES = {
    "black": 0x0000, "white": 0xFFFF, "red": 0xF800, "green": 0x07E0,
    "blue": 0x001F, "yellow": 0xFFE0, "cyan": 0x07FF, "magenta": 0xF81F,
    "gray": _rgb(128, 128, 128), "grey": _rgb(128, 128, 128),
    "orange": _rgb(255, 165, 0), "purple": _rgb(128, 0, 128),
}


def parse_color(v):
    """A color attribute value as RGB565, or None."""
    try:
        if v.startswith("#") and len(v) == 7:
            x = int(v[1:], 16)
            return _rgb(x >> 16, x >> 8 & 0xFF, x & 0xFF)
        if v.startswith("#") and len(v) == 4:
            x = int(v[1:], 16)
            return _rgb((x >> 8) * 17, (x >> 4 & 0xF) * 17, (x & 0xF) * 17)
        if v[:2] in ("0x", "0X") and 1 <= len(v) - 2 <= 4:
            return int(v[2:], 16)
        if v[:1].isdigit():
            n = int(v, 10)
            return n if n <= 0xFFFF else None
    except ValueError:
        return None
    return COLOR_NAMES.get(v.lower())


def _attr(attrs, name):
    i, n = 0, len(attrs)
    while i < n:
        while i < n and attrs[i] in " \t/":
            i += 1
        k0 = i
        while i < n and (attrs[i].isalnum() or attrs[i] == "-"):
            i += 1
        key = attrs[k0:i]
        if not key:
            return None
        value = ""
        if i < n and attrs[i] == "=":
            i += 1
            if i < n and attrs[i] in "\"'":
                q = attrs[i]
                end = attrs.find(q, i + 1)
                end = n if end < 0 else end
                value = attrs[i + 1:end]
                i = end + 1
            else:
                v0 = i
                while i < n and attrs[i] not in " \t/":
                    i += 1
                value = attrs[v0:i]
        if key.lower() == name:
            return value
    return None


def _entity(text, pos):
    end = text.find(";", pos + 1, pos + 11)  # rich.rs: 10 characters
    if end < 0:
        return None
    name = text[pos + 1:end]
    named = {"lt": "<", "gt": ">", "amp": "&", "quot": '"', "apos": "'", "nbsp": " "}
    if name in named:
        return named[name], end + 1
    try:
        if name[:2] in ("#x", "#X") and len(name) > 2:
            cp = int(name[2:], 16)
        elif name[:1] == "#" and name[1:].isdigit():
            cp = int(name[1:], 10)
        else:
            return None
    except ValueError:
        return None
    if cp > 0xFFFF:
        return None
    return chr(cp), end + 1


def chars(text):
    """Yield (char, flags, color or None) for each character, ("\\n", None, None)
    for a line break."""
    bold = italic = underline = 0
    colors = [None] * COLOR_DEPTH
    depth = 0
    pos, n = 0, len(text)

    def style():
        flags = (BOLD if bold else 0) | (ITALIC if italic else 0) | (UNDERLINE if underline else 0)
        top = min(depth, COLOR_DEPTH)
        return flags, (colors[top - 1] if top else None)

    while pos < n:
        ch = text[pos]
        if ch == "<":
            end = text.find(">", pos + 1, pos + 1 + TAG_MAX)
            body = text[pos + 1:end] if end >= 0 else None
            if body is not None:
                close = body.startswith("/")
                if close:
                    body = body[1:]
                k = 0
                while k < len(body) and body[k].isascii() and body[k].isalpha():
                    k += 1
                name, attrs = body[:k].lower(), body[k:]
                if k and (not attrs or attrs[0] in " \t/"):
                    pos = end + 1
                    step = -1 if close else 1
                    if name in ("b", "strong"):
                        bold = max(0, bold + step)
                    elif name in ("i", "em"):
                        italic = max(0, italic + step)
                    elif name == "u":
                        underline = max(0, underline + step)
                    elif name == "br":
                        yield "\n", None, None
                    elif name == "font":
                        if close:
                            depth = max(0, depth - 1)
                        elif depth < 255:
                            if depth < COLOR_DEPTH:
                                v = _attr(attrs, "color")
                                c = parse_color(v) if v is not None else None
                                colors[depth] = c if c is not None else (colors[depth - 1] if depth else None)
                            depth += 1
                    continue
        elif ch == "&":
            ent = _entity(text, pos)
            if ent:
                c, pos = ent
                yield (c, *style())
                continue
        elif ch == "\n":
            pos += 1
            yield "\n", None, None
            continue
        pos += 1
        yield (ch, *style())


def runs(text):
    """Lines of runs: [[(text, flags, color or None), ...], ...] (one line per
    break; consecutive characters of one style joined)."""
    lines = [[]]
    for ch, flags, color in chars(text):
        if ch == "\n":
            lines.append([])
            continue
        line = lines[-1]
        if line and line[-1][1] == flags and line[-1][2] == color:
            line[-1] = (line[-1][0] + ch, flags, color)
        else:
            line.append((ch, flags, color))
    return lines


def plain(text):
    """The text without markup (line breaks as \\n)."""
    return "\n".join("".join(r[0] for r in line) for line in runs(text))


if __name__ == "__main__":
    assert plain("a<b>b<i>c</b>d</i>e<u>f</u>") == "abcdef"
    assert [r[1] for r in runs("a<b>b<i>c</b>d</i>e")[0]] == [0, BOLD, BOLD | ITALIC, ITALIC, 0]
    assert runs('<font color="#FF0000">r<font color=blue>b</font></font>')[0] == [("r", 0, 0xF800), ("b", 0, 0x001F)]
    assert plain("a<br>b<BR/>c") == "a\nb\nc"
    assert plain("&lt;b&gt; &amp; &#65;&#x42; a<3 &bogus;") == "<b> & AB a<3 &bogus;"
    assert plain("<unknown x=1>t</unknown>") == "t"
    assert parse_color("orange") == 0xFD20 and parse_color("#0f0") == 0x07E0
    print("ok")
