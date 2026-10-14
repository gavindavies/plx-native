// Rect-fill vertex shader (pairs with fs_src.frag): unit quad -> pixel rect u_rect, flipped-Y NDC.
// v_uv is the unit-quad position; the fragment side derives both the gradient mix and the SDF
// coords from it.
attribute vec2 a_pos;
uniform vec4 u_rect;
uniform vec2 u_screen;
varying vec2 v_uv;
#ifdef PLX_DITHER_NC
// The dither tile's coordinate (`shaders/dither.glsl`, cost rule 4): target px / NOISE_DIM, linear
// in position, so interpolated exactly and never computed per fragment. Only the programs whose
// vertex source is built with `gfx::glsl_vs_dithered!` carry it — `fs_field.frag`, here.
varying highp vec2 v_dither_nc;
#endif
// The tilt scope (`gfx::set_tilt_scope`): (sin yaw, sin pitch, 1/perspective px) and the pivot.
uniform highp vec3 u_tilt;
uniform highp vec2 u_pivot;
void main(){
  v_uv = a_pos;
  highp vec2 px = u_rect.xy + a_pos * u_rect.zw;
#ifdef PLX_DITHER_NC
  v_dither_nc = px * (1.0 / 256.0);
#endif
  // TILT SCOPE (`gfx::set_tilt_scope`; `gfx::project_card_point` is this on the CPU): rotate the
  // point about the pivot, project it at distance 1/u_tilt.z and emit it with its `w`, so every
  // varying is interpolated perspective-correctly. Zero outside a scope: the upright path.
  highp float tw = 1.0;
  if (u_tilt.z > 0.0) {
    highp vec2 c = px - u_pivot;
    highp float cyw = sqrt(1.0 - u_tilt.x * u_tilt.x);
    highp float cp = sqrt(1.0 - u_tilt.y * u_tilt.y);
    highp float x1 = c.x * cyw;
    highp float z1 = -c.x * u_tilt.x;
    highp float y2 = c.y * cp - z1 * u_tilt.y;
    highp float z2 = c.y * u_tilt.y + z1 * cp;
    tw = 1.0 - z2 * u_tilt.z;
    px = u_pivot * tw + vec2(x1, y2);
  }
  vec2 ndc = px / u_screen * 2.0 - tw;
  gl_Position = vec4(ndc.x, -ndc.y, 0.0, tw);
}
