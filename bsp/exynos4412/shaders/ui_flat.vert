// A corner of a textured box (ferrite's GPU compositor, compose.rs): its position in pixels
// from the frame's top left, its texel, and a premultiplied colour the texel is multiplied by.
uniform vec2 scale;
attribute vec2 pos;
attribute vec2 uv;
attribute vec4 color;
varying vec2 v_uv;
varying vec4 v_color;

void main() {
    v_uv = uv;
    v_color = color;
    // scale = 2 / the frame's size: pixels to clip space, +y up (the frame's first line).
    gl_Position = vec4(pos.x * scale.x - 1.0, 1.0 - pos.y * scale.y, 0.0, 1.0);
}
