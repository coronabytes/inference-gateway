import {
  BLUE,
  CYAN,
  ICE,
  PEACH,
  PLAIN,
  SKY,
  STAR_HEAD,
  VIOLET,
  WHITE,
  between,
  gauss,
  pick,
  skyStars,
  type Rgb,
  type Scene,
  DISTANCE,
} from "./engine";
import type { Mat3 } from "./tilt";

/**
 * A pulsar (see engine.tsx for how a sky is drawn).
 *
 * A tiny, hot neutron star spins once every few seconds. Its magnetic axis is tilted against its
 * spin axis, so the two beams leaving its magnetic poles sweep round like a lighthouse. The beams
 * are streams of particles flying outwards; those far out left the star a moment ago, when it
 * pointed elsewhere, so each beam trails into a slight curve. Once per turn one beam sweeps across
 * the viewer and the star flashes. Around the star, particles run along the lines of its magnetic
 * field from pole to pole, turning with it.
 *
 * The star also blows a wind, as the pulsar in the Crab nebula does. In the plane of its equator
 * the wind shows as bright wisps: arcs that start at a sharp inner ring and travel outwards, much
 * brighter on the side where the wind blows towards the viewer. Along the spin axis it is
 * squeezed into two thin, knotty jets. All of that glows blue; around it hangs what is left of
 * the star that exploded, a cage of faint orange filaments.
 *
 * The beams, the wisps, the jets and the filaments are painted by the light's shader, with the
 * particles as sparks in them, and the bright parts glow into the dark (the engine's bloom).
 */

/** The angle between the magnetic axis and the spin axis (radians). */
const TILT = 0.66;
/** How fast the star spins (radians per second): one turn, and one flash, in about four seconds. */
const OMEGA = 1.6;
/** How far the beams reach, in scene radii, and how fast their particles cross that. */
const BEAM_LENGTH = 2.6;
const BEAM_SPEED = 0.22;
/** Seconds a beam particle needs per scene radius, which is how far the beam trails behind. */
const LAG = 0.07;

/** The resting view: the spin axis points up, a little to the left, and 40° towards the viewer, so
 * that a beam passes just by the line of sight once per turn. */
const REST_VIEW: Mat3 = (() => {
  const lean = 0.7;
  const turn = 1.75;
  const z = [Math.sin(lean) * Math.cos(turn), Math.sin(lean) * Math.sin(turn), Math.cos(lean)];
  const n = Math.hypot(z[2], z[0]);
  const x = [z[2] / n, 0, -z[0] / n];
  const y = [z[1] * x[2] - z[2] * x[1], z[2] * x[0] - z[0] * x[2], z[0] * x[1] - z[1] * x[0]];
  return [...x, ...y, ...z];
})();

/** Floats per star: position (3), colour and alpha, size, twinkle, phase, kind, and its part with
 * three numbers of its own (see the shader). */
const STRIDE = 15;
const BEAM = 1;
const FIELD = 2;
const WIND = 3;

function makePulsar(count: number, sky: number): Float32Array {
  const data: number[] = [];
  const add = (
    pos: [number, number, number],
    color: Rgb,
    alpha: number,
    size: number,
    move: [number, number, number, number] = [0, 0, 0, 0],
    twinkle = 0,
    kind = PLAIN,
  ) => {
    data.push(...pos, ...color, alpha, size, twinkle, Math.random() * Math.PI * 2, kind, ...move);
  };

  // The two beams: where along the beam a particle is at time 0, which pole it left from, and
  // where in the beam's cross-section it flies.
  for (let i = 0; i < count * 0.36; i++) {
    const bright = Math.random() < 0.04;
    add(
      [gauss(), gauss(), 0],
      pick([
        [CYAN, 3],
        [BLUE, 3],
        [VIOLET, 2],
        [ICE, 1],
      ]),
      bright ? between(0.6, 1) : between(0.3, 0.8),
      bright ? between(1.1, 1.8) : between(0.3, 0.8),
      [BEAM, Math.random(), i % 2 ? 1 : -1, 0],
    );
  }

  // The magnetic field: lines from pole to pole on three shells, sixteen around each.
  for (let i = 0; i < count * 0.26; i++) {
    const shell = pick([
      [0.16, 3],
      [0.3, 3],
      [0.5, 2],
    ]);
    const around = (Math.floor(Math.random() * 16) / 16) * Math.PI * 2 + shell * 2;
    add(
      [gauss(), gauss(), gauss()],
      pick([
        [CYAN, 3],
        [ICE, 2],
        [VIOLET, 2],
      ]),
      between(0.5, 1),
      between(0.4, 0.9),
      [FIELD, shell * (1 + gauss() * 0.012), around, Math.random()],
    );
  }

  // The wind in the plane of the equator: the angle, how far out at time 0, and the height.
  for (let i = 0; i < count * 0.38; i++) {
    const bright = Math.random() < 0.03;
    add(
      [Math.random() * Math.PI * 2, 0, gauss()],
      pick([
        [BLUE, 4],
        [VIOLET, 3],
        [ICE, 1.5],
        [PEACH, 1],
        [WHITE, 0.5],
      ]),
      bright ? between(0.5, 0.9) : between(0.3, 0.8),
      bright ? between(1.1, 2) : between(0.3, 0.75),
      [WIND, Math.random(), 0, 0],
      bright ? between(0.4, 1.6) : 0,
    );
  }

  for (const { pos, color, alpha, size } of skyStars(sky)) add(pos, color, alpha, size, undefined, 0, SKY);
  return new Float32Array(data);
}

/** The magnetic axis at the star's turn `a`, as both shaders compute it. */
const MAGNETIC = `vec3(${Math.sin(TILT).toFixed(4)} * cos(a), ${Math.sin(TILT).toFixed(4)} * sin(a), ${Math.cos(TILT).toFixed(4)})`;

const VERTEX = `
${STAR_HEAD}
attribute vec4 aMove;

vec3 magnetic(float time) {
  float a = ${OMEGA.toFixed(2)} * time;
  return ${MAGNETIC};
}

void main() {
  float size = aMisc.x * uPx;
  float alpha = aColor.a;
  vec3 color = aColor.rgb;
  vec2 px;
  if (aMisc.w > 1.5) {
    if (!skyStar(px)) {
      hide();
      return;
    }
  } else {
    vec3 p;
    if (aMove.x < 1.5) {
      // A beam particle flies straight out along the direction the pole pointed in when it left,
      // in a cone that widens with the distance. The beams grow out of the star as it lights up.
      float along = fract(aMove.y + uTime * ${BEAM_SPEED.toFixed(2)});
      float dist = 0.03 + ${BEAM_LENGTH.toFixed(1)} * along;
      vec3 axis = aMove.z * magnetic(uTime - dist * ${LAG.toFixed(2)});
      vec3 side = normalize(cross(axis, vec3(0.0, 0.0, 1.0)));
      p = axis * dist + (aPos.x * side + aPos.y * cross(axis, side)) * (0.004 + 0.03 * dist);
      alpha *= smoothstep(0.0, 0.03, along) * pow(1.0 - along, 1.5) * smoothstep(0.0, 0.25, uIntro * 1.3 - along);
      color = mix(vec3(0.9, 0.95, 1.0), color, smoothstep(0.0, 0.35, along));
    } else if (aMove.x < 2.5) {
      // A field line of a dipole: r = shell * sin²(angle from the magnetic axis). The particle runs
      // along it from one pole to the other, brightest near the poles.
      float theta = mix(0.1, PI - 0.1, fract(aMove.w + uTime * 0.035));
      float st = sin(theta);
      float r = aMove.y * st * st;
      vec3 axis = magnetic(uTime);
      vec3 side = normalize(cross(vec3(0.0, 0.0, 1.0), axis));
      p = r * st * (cos(aMove.z) * side + sin(aMove.z) * cross(axis, side)) + r * cos(theta) * axis + aPos * 0.004;
      alpha *= (0.25 + 0.75 * (1.0 - st * st)) * smoothstep(0.2, 0.9, uIntro);
    } else {
      // The wind drifts outwards in the equator's plane, lit where a ripple passes through it.
      float r = 0.12 + 1.05 * fract(aMove.y + uTime * 0.018);
      float angle = aPos.x + uTime * 0.05 / (r + 0.15);
      p = vec3(r * cos(angle), r * sin(angle), aPos.z * (0.008 + 0.05 * r));
      float wave = 0.5 + 0.5 * sin(r * 34.0 - uTime * 1.6 + 0.6 * sin(angle * 2.0));
      alpha *= 0.7 * (0.25 + 0.75 * wave * wave * wave) * smoothstep(0.12, 0.22, r) * (1.0 - smoothstep(0.55, 1.17, r));
      alpha *= smoothstep(0.3, 1.0, uIntro);
    }
    px = project(p, size);
    alpha *= twinkle();
  }
  star(px, size, alpha, color);
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
uniform float uTurn;
uniform float uFlow;
uniform float uCalm;
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

vec3 magnetic(float a) {
  return ${MAGNETIC};
}

/** Where the line of sight, from 'eye' in the direction 'ray', comes closest to the line through
 * the star along 'axis': how far along that line, and how far from it. */
vec2 nearest(vec3 eye, vec3 ray, vec3 axis) {
  float lean = dot(ray, axis);
  float ahead = dot(ray, eye);
  float up = dot(axis, eye);
  float open = max(1.0 - lean * lean, 0.0001);
  float along = (up - lean * ahead) / open;
  return vec2(along, length(eye + ray * (lean * up - ahead) / open - axis * along));
}

/** The glow of the beam from one magnetic pole: a cone of plasma that flickers as it streams
 * out. What is far out left the star earlier, when the pole pointed elsewhere, so the beam is
 * looked for where it was then. */
float beam(vec3 eye, vec3 ray, float pole) {
  vec2 at = nearest(eye, ray, pole * magnetic(uTurn));
  at = nearest(eye, ray, pole * magnetic(uTurn - ${(OMEGA * LAG).toFixed(3)} * max(at.x, 0.0)));
  if (at.x <= 0.0) return 0.0;
  float across = min(at.y / (0.006 + 0.03 * at.x), 6.0);
  float plasma = 0.6 + 0.4 * noise(vec3(at.x * 9.0 - uFlow * 2.2, pole * 3.0, 0.5));
  return exp(-across * across) * exp(-at.x / 0.8) * smoothstep(0.0, 0.04, at.x) * plasma * smoothstep(0.0, 0.3, uIntro * 1.3 - at.x / ${BEAM_LENGTH.toFixed(1)});
}

/** The glow of the two jets along the spin axis: thin, widening slowly, with knots that travel
 * outwards. The one that points away from the viewer is fainter. */
float jets(vec3 eye, vec3 ray) {
  vec2 at = nearest(eye, ray, vec3(0.0, 0.0, 1.0));
  float out_ = abs(at.x);
  float across = min(at.y / ((0.01 + 0.04 * out_) * (0.75 + 0.5 * noise(vec3(out_ * 6.0 - uFlow * 0.8, at.x > 0.0 ? 1.0 : 4.0, 2.0)))), 6.0);
  float knots = 0.5 + 0.5 * sin(out_ * 22.0 - uFlow * 3.0);
  return exp(-across * across) * smoothstep(0.02, 0.1, out_) * exp(-out_ / 0.5) * (0.45 + 0.55 * knots * knots) * (at.x > 0.0 ? 1.0 : 0.55);
}

/** The wind in the plane of the star's equator, where the line of sight crosses it. */
vec3 wind(vec3 eye, vec3 ray) {
  if (abs(ray.z) < 0.02) return vec3(0.0);
  vec3 hit = eye - ray * eye.z / ray.z;
  float r = length(hit.xy);
  if (r > 1.3) return vec3(0.0);
  float angle = atan(hit.y, hit.x);
  // The wisps: ripples that travel outwards (the same ones the wind's particles light up in),
  // each lit only along parts of its circle.
  float wave = 0.5 + 0.5 * sin(r * 34.0 - uFlow * 1.6 + 0.6 * sin(angle * 2.0));
  float arcs = 0.12 + 0.88 * smoothstep(0.3, 0.72, noise(vec3(cos(angle) * 2.5, sin(angle) * 2.5, r * 3.0 - uFlow * 0.15)));
  // The wind is fast: it shines much brighter where it blows towards the viewer.
  float boost = 1.0 / (1.0 + 0.36 * dot(hit.xy / max(r, 0.001), ray.xy));
  // The ring where the wind first piles up, and the haze the wisps travel through.
  float ring = min(abs(r - 0.17) / 0.012, 6.0);
  float body = smoothstep(0.1, 0.17, r) * exp(-r / 0.42);
  vec3 color = mix(vec3(0.7, 0.88, 1.0), vec3(0.42, 0.38, 1.0), smoothstep(0.15, 0.9, r));
  return color * ((1.3 * pow(wave, 5.0) * arcs + 0.1) * body + 0.7 * exp(-ring * ring)) * boost * boost * boost * 0.5;
}

/** The remains of the exploded star: thin orange filaments on a ragged shell around it all,
 * seen mostly towards its rim, and only in patches. */
vec3 remnant() {
  float far = length(vPos);
  if (far > 1.1) return vec3(0.0);
  // The shell reaches further in some directions than in others.
  vec3 outward = vec3(vPos / max(far, 0.001), 0.0) * uView;
  vec2 q = vPos / (0.72 + 0.3 * noise(outward * 1.7 + 3.0));
  float inside = 1.0 - dot(q, q);
  if (inside <= 0.0) return vec3(0.0);
  float depth = sqrt(inside);
  vec3 d = normalize(vec3(q, 0.45 + 0.55 * depth)) * uView;
  // Ridges of noise that is itself pushed about by noise: long, tangled strands.
  vec3 w = d * 2.0 + uFlow * 0.004;
  float push = noise(w);
  vec3 p = d * 3.4 + 1.6 * vec3(push, noise(w + 5.2), 1.0 - push);
  float strands = 0.0;
  float weight = 0.6;
  for (int i = 0; i < 3; i++) {
    float ridge = 1.0 - abs(2.0 * noise(p) - 1.0);
    ridge *= ridge;
    ridge *= ridge;
    strands += weight * ridge * ridge;
    p = p * 2.1 + 3.1;
    weight *= 0.65;
  }
  float patchy = smoothstep(0.3, 0.7, noise(d * 1.4 + 7.0));
  float rim = smoothstep(0.35, 0.9, length(q)) * smoothstep(0.0, 0.5, depth);
  vec3 color = mix(vec3(1.0, 0.3, 0.16), vec3(1.0, 0.5, 0.45), patchy);
  return color * max(strands - 0.04, 0.0) * rim * (0.12 + 0.88 * patchy) * 0.42;
}

void main() {
  // The line of sight through this pixel, as the scene sees it.
  vec3 eye = vec3(0.0, 0.0, ${DISTANCE.toFixed(1)}) * uView;
  vec3 ray = normalize(vec3(vPos, -${DISTANCE.toFixed(1)})) * uView;
  // The star, and the flash as a beam sweeps past the line of sight: the star flares up, with a
  // streak of light through it, as a camera's lens draws one.
  float flash = pow(max((uView * magnetic(uTurn)).z, 0.0), 24.0) * (1.0 - 0.75 * uCalm);
  float r = length(vPos);
  float core = min(r / 0.014, 8.0);
  float star = (1.5 * exp(-core * core) + 0.45 * exp(-r / 0.035) + 0.08 * exp(-r / 0.25)) * (1.0 + 2.5 * flash);
  star += flash * (0.2 * exp(-r / 0.5) + 0.7 * exp(-abs(vPos.y) / 0.005 - abs(vPos.x) / 0.45));
  vec3 light = vec3(0.78, 0.88, 1.0) * star * smoothstep(0.0, 0.6, uIntro);
  light += vec3(0.6, 0.76, 1.0) * 0.6 * (beam(eye, ray, 1.0) + beam(eye, ray, -1.0));
  float blowing = smoothstep(0.3, 1.2, uIntro);
  light += (wind(eye, ray) + vec3(0.62, 0.8, 1.0) * 0.55 * jets(eye, ray)) * blowing;
  light += remnant() * smoothstep(0.6, 1.8, uIntro);
  gl_FragColor = vec4(1.0 - exp(-light * 1.4), 1.0);
}
`;

export const pulsar: Scene = {
  vertex: VERTEX,
  glow: GLOW,
  extent: 6,
  stride: STRIDE,
  layout: [
    ["aPos", 3, 0],
    ["aColor", 4, 3],
    ["aMisc", 4, 7],
    ["aMove", 4, 11],
  ],
  rest: REST_VIEW,
  introSeconds: 3,
  stillTime: 1.2,
  // The star, its beams and the wisps glow into the dark around them.
  bloom: 0.7,
  make: makePulsar,
  // The star's turn and the time everything else moves by, both kept small for the light's
  // shader. The time starts over after a whole number of ripples of the wind, so that the wisps
  // stay where the wind's particles light up.
  // Behind the console's pages the flash is much fainter.
  frame: (time, _intro, _still, backdrop) => ({
    uTurn: (OMEGA * time) % (Math.PI * 2),
    uFlow: time % ((Math.PI * 2000) / 1.6),
    uCalm: backdrop ? 1 : 0,
  }),
};
