// The texel times the colour: premultiplied, for blending "over" what the frame holds.
// The texel coordinate goes to the sampler unchanged, so it keeps the varying's precision.
precision mediump float;
uniform sampler2D t;
varying vec2 v_uv;
varying vec4 v_color;

void main() {
    gl_FragColor = texture2D(t, v_uv) * v_color;
}
