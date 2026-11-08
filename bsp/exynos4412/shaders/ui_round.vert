// A corner of one piece of a box (ferrite's GPU compositor, compose.rs): its position in
// pixels from the frame's top left; in `shape`, where it is against the box's nearest rounded
// corner (xy, pixels, positive outwards past the corner circle's centre), the corner radius (z)
// and the border width (w); in `arc`, constants of the piece's corner for the fragment
// shader (1/r, r/2, (r/ri)^2, ri/2 with ri = r - border, 0 = not rounded); the fill and the
// border colour, premultiplied.
uniform vec2 scale;
attribute vec2 pos;
attribute vec4 shape;
attribute vec4 arc;
attribute vec4 fill;
attribute vec4 border;
varying vec4 v_shape;
varying vec4 v_arc;
varying vec4 v_fill;
varying vec4 v_border;

void main() {
    v_shape = shape;
    v_arc = arc;
    v_fill = fill;
    v_border = border;
    gl_Position = vec4(pos.x * scale.x - 1.0, 1.0 - pos.y * scale.y, 0.0, 1.0);
}
