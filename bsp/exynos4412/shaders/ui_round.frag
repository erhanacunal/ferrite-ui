// A rounded box's coverage at the pixel's centre, anti-aliased over one pixel: the fill inside
// the border's inner edge, the border between the two edges.
//
// No length()/sqrt: on the board the PP's sqrt gave wrong coverage in scattered 2x2 quads.
// Past a corner circle's centre (both q >= 0) the distance to a circle of radius r is
// len - r ~ (len^2 / r^2 - 1) * r / 2, exact on the edge and within 1/(2r) pixels of it one
// pixel away; the squares are taken in units of r, so mediump does not overflow. Elsewhere (and
// for square corners) it is max(q.x, q.y) - r. Pieces away from the corners have a large
// negative x or y.
precision mediump float;
varying vec4 v_shape;
varying vec4 v_arc;
varying vec4 v_fill;
varying vec4 v_border;

void main() {
    vec2 q = v_shape.xy;
    float lin = max(q.x, q.y) - v_shape.z;
    vec2 m = max(q, 0.0) * v_arc.x;
    float l2 = dot(m, m);
    float corner = step(0.0, min(q.x, q.y));
    float d_outer = mix(lin, (l2 - 1.0) * v_arc.y, corner * step(0.0001, v_arc.x));
    float d_inner = mix(lin + v_shape.w, (l2 * v_arc.z - 1.0) * v_arc.w, corner * step(0.0001, v_arc.w));
    float outer = clamp(0.5 - d_outer, 0.0, 1.0);
    float inner = clamp(0.5 - d_inner, 0.0, 1.0);
    gl_FragColor = v_fill * inner + v_border * (outer - inner);
}
