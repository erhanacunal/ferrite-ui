// A rounded box's coverage at the pixel's centre from its signed distance (the rounded box
// distance: length of the outside part, plus the inside part, minus the radius), anti-aliased
// over one pixel: the fill inside the border's inner edge, the border between the two edges.
// Pieces away from the corners have a large negative x or y: the distance is then the other's.
precision mediump float;
varying vec4 v_shape;
varying vec4 v_fill;
varying vec4 v_border;

void main() {
    vec2 q = v_shape.xy;
    float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - v_shape.z;
    float outer = clamp(0.5 - d, 0.0, 1.0);
    float inner = clamp(0.5 - d - v_shape.w, 0.0, 1.0);
    gl_FragColor = v_fill * inner + v_border * (outer - inner);
}
