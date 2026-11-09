import { STAR_HEAD, type Scene } from "./engine";
import { multiply, rotateX, type Mat3 } from "./tilt";

/**
 * A black hole (see engine.tsx for how a sky is drawn).
 *
 * A black hole in a thick, dusty disc of glowing gas, seen from a little above the disc's plane.
 * Its gravity bends light, so what shows is not what is there: the far half of the disc appears
 * lifted over the top of the hole and folded under it, so that a bright ring of gas surrounds the
 * hole's dark shadow, with the near half of the disc as a dark band across it. The gas is
 * cream-white close to the hole, where it is hottest, orange further out and a dark brown at the
 * rim, in a wide blue haze. It orbits at a good part of the speed of light, which makes the side
 * coming towards the viewer brighter than the side going away. Behind the hole, a little above
 * it, stands a bright star with a long streak of light through it: the hole shows it twice,
 * above and below, and bends the streak into a long arc from one to the other.
 *
 * The bright parts glow into the dark around them (the engine's bloom), the hole's shadow
 * included.
 *
 * Everything is painted by the light's shader, which follows the path of the light that reaches
 * each pixel back around the hole and through the gas (see the shader). There are no stars of the
 * usual kind: the sky is painted too, so that it bends.
 */

/** Lengths in the scene are in Schwarzschild radii: the hole's horizon has radius 1. */

/** How far the camera is from the hole when the sky's zoom is 3; it moves with the zoom, so that
 * the hole keeps its size on screen. */
const CAMERA = 33;
/** The radius of the hole's shadow on screen, in scene radii: light that passes the hole closer
 * than 2.6 falls in. */
const SHADOW = (2.598 * 3) / CAMERA;
/** The disc's inner and outer edge. Inside 3, nothing orbits a black hole for long; the gas
 * between there and the inner edge is on its way in. */
const DISC_IN = 2.2;
const DISC_OUT = 9;
/** How thick the disc is: its gas reaches about this high above its plane, at the hole and per
 * unit of distance from it. */
const DISC_HEIGHT = [0.03, 0.1];
/** How fast the gas orbits at the distance 1 (radians per second); it slows down outwards. */
const ORBIT_SPEED = 1.6;
/** Seconds after which the drifting pattern of the gas starts afresh (see the shader). */
const FLOW_SECONDS = 12;
/** Between these distances from the middle of the screen, in scene radii, lies the edge of the
 * hole's shadow, which is too sharp for one look per pixel (see the shader). */
const SHARP = [SHADOW - 0.05, SHADOW + 0.05];

function rotateZ(angle: number): Mat3 {
  const c = Math.cos(angle);
  const s = Math.sin(angle);
  return [c, s, 0, -s, c, 0, 0, 0, 1];
}
/** The resting view: from 16° above the disc's plane, with the disc very slightly askew. From
 * there the near half of the disc crosses the picture below the hole as a wide band, dark at its
 * thick outer edge and brighter towards the hole. */
const REST_VIEW = multiply(rotateZ(0.04), rotateX(-1.29));

/** Where the bright star is, as a direction from the camera: behind the hole, a little above it
 * and a touch to the left. It stays there on screen, also while the view leans towards the
 * mouse: a small move of the star against the hole would change its bent images a great deal. */
const STAR = (() => {
  const x = -0.02;
  const y = 0.07;
  const n = Math.hypot(x, y, 1);
  return [x / n, y / n, -1 / n].map((v) => v.toFixed(5)).join(", ");
})();

const VERTEX = `
${STAR_HEAD}
void main() {
  hide();
}
`;

const GLOW = `
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform mat3 uView;
uniform float uIntro;
uniform float uZoom;
uniform float uPixel;
uniform float uFlow;
varying vec2 vPos;

float hash(vec3 p) {
  p = fract(p * 0.3183099 + 0.1) * 17.0;
  return fract(p.x * p.y * p.z * (p.x + p.y + p.z));
}

/** Smooth noise in space, from 0 to 1. */
float noise(vec3 x) {
  vec3 i = floor(x);
  vec3 f = fract(x);
  f = f * f * (3.0 - 2.0 * f);
  return mix(
    mix(mix(hash(i), hash(i + vec3(1.0, 0.0, 0.0)), f.x), mix(hash(i + vec3(0.0, 1.0, 0.0)), hash(i + vec3(1.0, 1.0, 0.0)), f.x), f.y),
    mix(mix(hash(i + vec3(0.0, 0.0, 1.0)), hash(i + vec3(1.0, 0.0, 1.0)), f.x), mix(hash(i + vec3(0.0, 1.0, 1.0)), hash(i + vec3(1.0, 1.0, 1.0)), f.x), f.y),
    f.z
  );
}

/** How thick the disc is at the distance 'far' from the hole. */
float thickness(float far) {
  return ${DISC_HEIGHT[0].toFixed(3)} + ${DISC_HEIGHT[1].toFixed(3)} * far;
}

/** The disc's gas at the point 'at', for light flying through it in the direction 'heading': the
 * colour it glows in, and how dense it is. */
vec4 gas(vec3 at, vec3 heading) {
  float far = length(at.xy);
  float up = at.z / thickness(far);
  float within = smoothstep(${(DISC_IN * 0.8).toFixed(2)}, ${(DISC_IN * 1.2).toFixed(2)}, far) * (1.0 - smoothstep(${(DISC_OUT * 0.5).toFixed(2)}, ${DISC_OUT.toFixed(1)}, far));
  float around = atan(at.y, at.x);
  // Streaks drawn out along the orbit. The inner gas orbits faster than the outer, which would
  // wind any pattern up ever tighter. So the pattern is drawn twice, each copy wound for a while
  // and then started afresh, and the two are blended so that a copy is never seen while it starts
  // over.
  float turn = ${ORBIT_SPEED.toFixed(1)} * pow(far, -1.5) * ${FLOW_SECONDS.toFixed(1)};
  float first = uFlow / ${FLOW_SECONDS.toFixed(1)};
  float second = first + 0.5;
  float wound = around - turn * (fract(first) - 0.5);
  float streaks = noise(vec3(far * 2.4 + floor(first) * 13.7, vec2(cos(wound), sin(wound)) * 1.6 + up * 0.35));
  wound = around - turn * (fract(second) - 0.5);
  streaks = mix(streaks, noise(vec3(far * 2.4 + floor(second) * 13.7 + 6.9, vec2(cos(wound), sin(wound)) * 1.6 + up * 0.35)), abs(2.0 * fract(first) - 1.0));
  // Fine dust, which turns with the gas half-way out.
  wound = around - uFlow * 0.07;
  float dust = noise(vec3(far * 13.0, vec2(cos(wound), sin(wound)) * 11.0 + up * 1.5));
  // The disc is no haze: it has a surface, ragged where the streaks and the dust are thin, and
  // below that it is dense.
  float level = abs(up) + 0.8 * (0.5 - streaks) + 0.45 * (0.5 - dust);
  float dense = within * (1.0 - smoothstep(0.7, 1.0, level)) * (0.5 + 1.2 * streaks) * (0.7 + 0.6 * dust);

  // The gas moves at 'speed' (of light) along its orbit. Light from gas that comes towards the
  // viewer is brighter, light from gas that goes away dimmer, and all of it loses energy climbing
  // away from the hole. Less is made of that here than there would be, to keep the whole ring of
  // gas around the hole aglow.
  float speed = sqrt(0.5 / far);
  vec3 orbit = vec3(-sin(around), cos(around), 0.0);
  float shift = sqrt(1.0 - speed * speed) / (1.0 + speed * dot(orbit, normalize(heading))) * sqrt(max(1.0 - 1.0 / far, 0.0));
  // Hotter towards the hole: cream-white there, orange further out, a dark brown at the rim.
  float heat = pow(${DISC_IN.toFixed(1)} / far, 0.75);
  float seen = heat * (0.55 + 0.6 * shift);
  vec3 color = mix(vec3(0.3, 0.1, 0.035), vec3(0.95, 0.42, 0.11), smoothstep(0.36, 0.64, seen));
  color = mix(color, vec3(1.0, 0.88, 0.6), smoothstep(0.62, 0.95, seen));
  // Lit and dark grains of dust side by side.
  float lit = (0.03 + 5.5 * pow(heat, 4.0) * pow(shift, 1.5)) * (0.55 + 0.9 * dust * streaks * 2.0);
  return vec4(color * lit, dense);
}

/** The stars of the sky in the direction 'd': one, at most, in each cell of a grid laid over the
 * six faces of a cube around the viewer.
 *
 * A star is a point, and a point stays a point however its light is bent: it only gets brighter
 * or fainter. Painted as a small disc, though, it would be bent like one, into a dash along the
 * circle around the hole, and a sky full of dashes looks smeared. So each star's disc is squeezed
 * beforehand by as much as the hole is about to stretch it: 'deep' is how much the hole stretches
 * things along the line away from it, as seen from behind it along 'axis', and 'wide' how much
 * around it. */
vec3 stars(vec3 d, vec3 axis, float deep, float wide) {
  vec3 size = abs(d);
  float face = size.x >= size.y && size.x >= size.z ? 1.0 : size.y >= size.z ? 2.0 : 3.0;
  vec3 q = face < 1.5 ? d.yzx : face < 2.5 ? d.zxy : d.xyz;
  vec2 at = q.xy / abs(q.z);
  vec2 cell = floor(at * 40.0);
  vec3 seed = vec3(cell, face * sign(q.z) * 3.0);
  if (hash(seed) < 0.8) return vec3(0.0);
  vec2 where = (cell + 0.5 + 0.6 * (vec2(hash(seed + 1.3), hash(seed + 2.7)) - 0.5)) / 40.0;
  vec3 here = normalize(vec3(at, 1.0));
  vec3 off = here - normalize(vec3(where, 1.0));
  // The same, seen from the cube's face.
  vec3 middle = face < 1.5 ? axis.yzx : face < 2.5 ? axis.zxy : axis.xyz;
  if (q.z < 0.0) middle.z = -middle.z;
  vec3 away = here - middle * dot(here, middle);
  float tilt = length(away);
  if (tilt > 0.001) {
    away /= tilt;
    vec3 round_ = cross(middle, away);
    off += away * dot(off, away) * (deep - 1.0) + round_ * dot(off, round_) * (wide - 1.0);
  }
  float bright = hash(seed + 4.1);
  vec3 color = mix(vec3(1.0, 0.74, 0.58), vec3(0.82, 0.91, 1.0), smoothstep(0.2, 0.6, hash(seed + 5.9)));
  // Brighter where the hole gathers its light, fainter where it spreads it.
  return color * (0.15 + 0.85 * bright * bright) * min(deep * wide, 4.0) * exp(-dot(off, off) / (0.0000005 + 0.0000014 * bright));
}

/** The bright star, seen in the direction 'd': a white-hot core in a soft, pale blue glow, with a
 * single long, thin streak of light through it, as a camera's lens draws one. The hole bends the
 * streak with the star: into a long arc that sweeps around it. */
vec3 brightStar(vec3 d) {
  // The view's inverse (its transpose) takes the direction on screen into the scene.
  vec3 to = vec3(${STAR}) * uView;
  if (dot(d, to) < 0.0) return vec3(0.0);
  vec3 off = d - to;
  float far = length(off);
  vec3 across = normalize(cross(to, vec3(0.0, 0.0, 1.0)));
  float x = abs(dot(off, across));
  float y = abs(dot(off, cross(to, across)));
  float core = min(far / 0.005, 8.0);
  float glow = 0.35 * exp(-far / 0.01) + 0.05 * exp(-far / 0.05);
  // The streak: a fine line, in a short, wider and fainter one.
  glow += 0.7 * exp(-y / 0.0009 - x / 0.16) + 0.1 * exp(-y / 0.003 - x / 0.05);
  return vec3(0.85, 0.92, 1.0) * 3.0 * exp(-core * core) + vec3(0.5, 0.64, 1.0) * glow;
}

/** What is seen at the place 'at' on screen. The camera looks at the hole from some way off; the
 * light that arrives there is followed backwards from the camera: past the hole, into it, or
 * around it, and through the gas of the disc on its way. */
vec3 look(vec2 at) {
  float camera = ${CAMERA.toFixed(1)} * uZoom / 3.0;
  vec3 p = vec3(0.0, 0.0, camera) * uView;
  vec3 v = normalize(vec3(at, -uZoom)) * uView;
  // The direction straight through the hole, and how the hole stretches what is seen this far
  // off that direction: along the line away from it, and around it. Light from right behind the
  // hole arrives from a ring around it, and there the stretch around it has no end.
  vec3 axis = -normalize(p);
  float ring = 2.0 / camera;
  float off = dot(at, at) / (uZoom * uZoom);
  float deep = 1.0 / (1.0 + ring / max(off, 0.00001));
  float wide = min(1.0 / abs(1.0 - ring / max(off, 0.00001)), 8.0);
  // How far the light would pass from the hole if it flew straight, squared. Around a black hole,
  // light flies as if it were pulled towards the hole by 1.5 * that / distance^4.
  vec3 swing = cross(p, v);
  float pull = 1.5 * dot(swing, swing);
  // Each pixel takes steps of a slightly different length through the gas, which turns the
  // steps, that would show as bands, into a fine grain: dust.
  float grain = 0.3 + 0.2 * hash(vec3(gl_FragCoord.xy, 7.0));
  float on = smoothstep(0.0, 1.0, uIntro);
  vec3 light = vec3(0.0);
  // How much of what lies further along the path still shows through the gas passed so far.
  float clear = 1.0;
  // The same, at the moment the light reaches the hole: how much of what is at the hole shows
  // through the near half of the disc.
  float veil = 1.0;
  bool reached = false;
  bool escaped = false;
  for (int i = 0; i < 200; i++) {
    float r2 = dot(p, p);
    float r = sqrt(r2);
    // Fallen into the hole: nothing comes from there.
    if (r < 1.0) break;
    // Past the hole and away: what shows is the sky in that direction.
    if (r > camera * 1.02 && dot(p, v) > 0.0) {
      escaped = true;
      break;
    }
    if (!reached && r < 4.0) {
      reached = true;
      veil = clear;
    }
    // Small steps close to the hole, where the path bends, and long ones far from it; inside
    // the disc, steps small enough to see its gas by.
    float step_ = r * mix(0.06, 0.35, smoothstep(4.0, 25.0, r));
    float far = length(p.xy);
    float thick = thickness(far);
    bool inside = far > ${(DISC_IN * 0.8).toFixed(2)} && far < ${DISC_OUT.toFixed(1)} && abs(p.z) < 1.9 * thick;
    if (inside) step_ = min(step_, thick * grain);
    v -= pull * p / (r2 * r2 * r) * step_;
    if (inside) {
      // The gas glows, and hides what lies behind it.
      vec4 here = gas(p + v * step_ * 0.5, v);
      float hidden = (1.0 - exp(-here.a * step_ * 4.5)) * on;
      light += clear * here.rgb * hidden;
      clear *= 1.0 - hidden;
      // Behind this much gas nothing shows any more.
      if (clear < 0.02) break;
    }
    p += v * step_;
  }
  // Light that never got as far as the hole was stopped by the gas in front of it.
  if (!reached) veil = clear;
  if (escaped) {
    vec3 d = normalize(v);
    // Behind the stars, faint blue clouds, which bend like everything else.
    light += clear * (stars(d, axis, deep, wide) + brightStar(d) + vec3(0.02, 0.035, 0.1) * smoothstep(0.3, 0.9, noise(d * 2.5 + 3.0)));
  }
  // A faint warm haze close around the hole, and a wide blue one, densest in a band where the
  // hole gathers the light of the sky behind it.
  float out_ = length(at);
  float band = (out_ - 0.74) / 0.16;
  light += vec3(1.0, 0.5, 0.2) * 0.04 * exp(-out_ / 0.5) * on;
  light += vec3(0.07, 0.11, 0.4) * (0.13 * exp(-out_ / 0.8) + 0.09 * exp(-band * band)) * on;
  // Light that circled the hole many times before it got away piles up at the edge of the
  // shadow, in rings too thin to draw: a thin glow there stands for them. Like the disc it comes
  // from, it is brighter on the side where the gas comes towards the viewer, and the near half of
  // the disc hides it where it crosses in front.
  float rim = out_ - ${SHADOW.toFixed(3)};
  float coming = 0.5 + 0.5 * dot(at / max(out_, 0.001), normalize((uView * vec3(-1.0, 0.0, 0.0)).xy));
  light += vec3(1.0, 0.86, 0.6) * 0.4 * (0.45 + 0.55 * coming * coming) * exp(-abs(rim) / (rim < 0.0 ? 0.005 : 0.012)) * on * veil;
  // More contrast than the light itself has: the dark parts darker and the bright ones brighter.
  vec3 seen = 1.0 - exp(-light * 1.6);
  return mix(seen, seen * seen * (3.0 - 2.0 * seen), 0.75);
}

void main() {
  // At the edge of the hole's shadow the picture changes within a pixel, from the brightest gas
  // to black. One look per pixel leaves that jagged, so there each pixel is the average of four
  // looks, spread over it.
  float far = length(vPos);
  bool fine = far > ${SHARP[0].toFixed(3)} && far < ${SHARP[1].toFixed(3)};
  vec3 seen = vec3(0.0);
  for (int i = 0; i < 4; i++) {
    vec2 shift = i == 0 ? vec2(0.125, 0.375) : i == 1 ? vec2(-0.375, 0.125) : i == 2 ? vec2(-0.125, -0.375) : vec2(0.375, -0.125);
    seen += look(fine ? vPos + uPixel * shift : vPos);
    if (!fine) break;
  }
  if (fine) seen *= 0.25;
  gl_FragColor = vec4(seen * smoothstep(0.0, 0.25, uIntro), 1.0);
}
`;

export const blackhole: Scene = {
  vertex: VERTEX,
  glow: GLOW,
  // The whole screen.
  extent: 6,
  stride: 11,
  layout: [
    ["aPos", 3, 0],
    ["aColor", 4, 3],
    ["aMisc", 4, 7],
  ],
  rest: REST_VIEW,
  introSeconds: 3,
  // Following the light costs far more than drawing stars does: 30 frames a second are plenty.
  gap: 30,
  // The disc and the bright star glow into the dark around them, the hole's shadow included.
  bloom: 0.6,
  make: () => new Float32Array(0),
  // The time the gas drifts by; kept small for the light's shader.
  frame: (time) => ({ uFlow: time % 3600 }),
};
