import {
  BLUE,
  ICE,
  ORANGE,
  PINK,
  PLAIN,
  SKY,
  STAR_HEAD,
  VIOLET,
  WHITE,
  between,
  direction,
  pick,
  skyStars,
  type Rgb,
  type Scene,
} from "./engine";

/**
 * A supernova and the remnant it leaves (see engine.tsx for how a sky is drawn).
 *
 * A red giant shines for a moment, then explodes in a white flash. Its gas is thrown outwards as a
 * shell, fast at first and slowing down, white-hot at first and cooling into a tangle of glowing
 * filaments: pink and orange where the shell is densest, violet where it thins, in a haze of blue
 * gas. The blast was not the same in every direction, so one side of the remnant burns much
 * brighter than the other, and the gas keeps churning slowly. Knots of stars glitter inside it and
 * a small neutron star stays behind in the middle. After a minute or so the remnant fades and the
 * next blast goes off.
 *
 * The gas is not made of stars: it is painted by the light's shader, as two layers of noise on the
 * near and the far side of the shell, which turn with the view like the stars do.
 */

/** Seconds the star shines before the first blast. */
const DELAY = 2.2;
/** Seconds from one blast to the next. */
const CYCLE = 80;
/** The shell's radius at the end of its life, in scene radii. */
const REACH = 1;
/** How quickly the shell slows down: its radius is REACH * (1 - e^(-SLOWING * age)) / (1 - e^(-SLOWING)). */
const SLOWING = 6;
/** The remnant turns slowly on its axis (radians per second). */
const SPIN_SPEED = 0.006;
/** The resting view: tipped back a little. */
const REST_VIEW = [0.9553, -0.0873, 0.2823, 0, 0.9553, 0.2955, -0.2955, -0.2823, 0.9127];
/** The side of the shell that burns brightest: towards the lower left and the viewer, at rest. */
const HOT = (() => {
  const v = [-0.68, -0.6, 0.42];
  const m = REST_VIEW;
  // The view's inverse (its transpose) takes the direction on screen back into the scene.
  return [0, 1, 2].map((i) => (m[i * 3] * v[0] + m[i * 3 + 1] * v[1] + m[i * 3 + 2] * v[2]).toFixed(4)).join(", ");
})();

/** Floats per star: direction (3), colour and alpha, size, twinkle, phase, kind, then how far out
 * it flies (as a share of the shell's radius) and a seed for its drift. */
const STRIDE = 13;

const MAGENTA: Rgb = [1, 0.42, 0.78];

function makeSupernova(count: number, sky: number): Float32Array {
  const data: number[] = [];
  const add = (dir: [number, number, number], color: Rgb, alpha: number, size: number, reach: number, twinkle = 0, kind = PLAIN) => {
    data.push(...dir, ...color, alpha, size, twinkle, Math.random() * Math.PI * 2, kind, reach, Math.random() * 6.28);
  };

  // Fine dust inside the remnant, most of it far out.
  for (let i = 0; i < count * 0.4; i++) {
    add(
      direction(),
      pick([
        [PINK, 3],
        [MAGENTA, 2],
        [ORANGE, 2],
        [VIOLET, 2],
        [BLUE, 1],
        [ICE, 1],
      ]),
      between(0.12, 0.45),
      between(0.3, 0.7),
      0.72 * Math.random() ** 0.35,
    );
  }
  // Bright knots that glitter in the gas.
  for (let i = 0; i < count * 0.02; i++) {
    add(
      direction(),
      pick([
        [WHITE, 3],
        [PINK, 2],
        [ICE, 2],
      ]),
      between(0.5, 1),
      between(1.1, 2.2),
      0.7 * Math.random() ** 0.5,
      between(0.4, 1.8),
    );
  }

  for (const { pos, color, alpha, size } of skyStars(sky)) add(pos, color, alpha, size, 0, 0, SKY);
  return new Float32Array(data);
}

/** The shell's radius at 'age' (0 to 1) and how much of its light is left, in both shaders. */
const SHELL = `
float radius(float age) {
  return ${REACH.toFixed(2)} * (1.0 - exp(-${SLOWING.toFixed(1)} * age)) / ${(1 - Math.exp(-SLOWING)).toFixed(4)};
}

float fading(float age) {
  return smoothstep(0.0, 0.01, age) * (1.0 - smoothstep(0.8, 1.0, age));
}
`;

const VERTEX = `
${STAR_HEAD}
attribute vec2 aMove;
uniform float uSpin;
uniform float uAge;
${SHELL}
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
    // Nothing shows before the first blast.
    if (uAge < 0.0) {
      hide();
      return;
    }
    float r = radius(uAge) * aMove.x;
    // Thrown straight out, with a slow drift that grows as the gas thins.
    vec3 p = aPos * r;
    float seed = aMove.y;
    p += 0.03 * r * vec3(sin(seed + uAge * 9.0), cos(seed * 1.7 + uAge * 8.0), sin(seed * 2.3 - uAge * 8.5));
    float c = cos(uSpin);
    float s = sin(uSpin);
    p.xy = vec2(c * p.x - s * p.y, s * p.x + c * p.y);
    // White-hot and bright just after the blast; then each star cools to its own colour.
    alpha *= fading(uAge) * (1.0 + 3.0 * exp(-uAge * 30.0));
    color = mix(vec3(0.85, 0.92, 1.0), color, smoothstep(0.01, 0.1, uAge));
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
uniform float uStar;
uniform float uAge;
uniform float uSpin;
uniform float uChurn;
varying vec2 vPos;
${SHELL}
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

/** A direction on screen, back in the scene. */
vec3 unview(vec3 v) {
  vec3 d = v * uView;
  float c = cos(uSpin);
  float s = sin(uSpin);
  return vec3(c * d.x + s * d.y, c * d.y - s * d.x, d.z);
}

/** The light of the shell's gas in the direction 'd' from its centre. */
vec3 gas(vec3 d) {
  // How hard the blast was this way: the hot side is dense with bright filaments, the far side is
  // mostly thin blue gas.
  float hot = smoothstep(-0.7, 0.75, dot(d, vec3(${HOT})));
  float thin = noise(d * 1.7 + 7.0);
  float patchy = smoothstep(0.25, 0.6, thin);
  // The filaments are the ridges of noise that is itself pushed about by noise, which pulls them
  // into long, tangled strands. Finer ridges are laid over the coarse ones.
  vec3 w = d * 2.1 + uChurn;
  // Squeezed along one axis, so that the strands run mostly one way, as if combed.
  vec3 p = (d * 3.3 + 1.8 * vec3(noise(w), noise(w + 5.2), noise(w.zxy + 9.7))) * vec3(1.3, 0.42, 1.0);
  float strands = 0.0;
  float weight = 0.5;
  for (int i = 0; i < 5; i++) {
    float ridge = 1.0 - abs(2.0 * noise(p) - 1.0);
    ridge *= ridge * ridge;
    strands += weight * ridge * ridge;
    p = p * 2.1 + 3.1;
    weight *= 0.72;
  }
  float glow = max(strands - 0.06, 0.0) * (0.12 + 1.25 * hot) * (0.25 + 0.75 * patchy);
  // Violet where the gas is faint, orange and then pale pink where it is dense.
  vec3 color = mix(vec3(0.55, 0.16, 0.95), vec3(1.0, 0.36, 0.3), smoothstep(0.1, 0.55, glow));
  color = mix(color, vec3(1.0, 0.74, 0.86), smoothstep(0.5, 1.2, glow));
  float haze = thin * (0.35 + 0.65 * noise(d * 4.3 + 2.0));
  return color * glow + vec3(0.14, 0.09, 0.62) * haze * haze * (1.35 - hot);
}

void main() {
  float r = length(vPos);
  // The red giant before the first blast, and the neutron star left behind after it.
  float giant = min(r / 0.022, 8.0);
  float dwarf = min(r / 0.008, 8.0);
  vec3 light = vec3(1.0, 0.72, 0.5) * uStar * (1.4 * exp(-giant * giant) + 0.4 * exp(-r / 0.07) + 0.06 * exp(-r / 0.3))
    + vec3(0.8, 0.9, 1.0) * step(0.0, uAge) * (0.7 * exp(-dwarf * dwarf) + 0.1 * exp(-r / 0.03));
  if (uAge >= 0.0) {
    float edge = radius(uAge);
    float fade = fading(uAge);
    // The flash.
    light += vec3(0.85, 0.92, 1.0) * exp(-uAge * 60.0) * (2.0 * exp(-r / 0.1) + 0.5 * exp(-r / 0.45));
    // A red glow in the space the hot side is ploughing into.
    vec2 ahead = (uView * vec3(${HOT})).xy;
    float facing = smoothstep(-0.2, 0.9, dot(vPos / max(r, 0.001), normalize(ahead)));
    light += vec3(0.5, 0.04, 0.03) * fade * 0.4 * facing * exp(-max(r - edge * 0.75, 0.0) / 0.2) * smoothstep(0.0, 0.5, edge);
    // The remnant is no ball: the gas got further in some directions than in others, so its
    // outline is ragged, and it thins out towards the outline in wisps.
    vec3 outward = unview(vec3(vPos / max(r, 0.001), 0.0));
    float reach = edge * (0.66 + 0.42 * noise(outward * 1.6 + 3.0) + 0.1 * noise(outward * 5.0 + 8.0));
    vec2 q = vPos / max(reach, 0.001);
    float inside = 1.0 - dot(q, q);
    if (inside > 0.0) {
      // The shell is seen twice at every point: its near and its far side.
      float depth = sqrt(inside);
      vec3 near = unview(normalize(vec3(q, 0.5 + 0.5 * depth)));
      vec3 shell = gas(near) + 0.6 * gas(unview(normalize(vec3(q, -0.5 - 0.5 * depth))));
      shell *= smoothstep(0.0, 0.7, depth + 0.5 * noise(near * 6.0 + 4.0) - 0.25);
      // White-hot at first.
      float heat = exp(-uAge * 22.0);
      shell = mix(shell, vec3(dot(shell, vec3(0.5))) * vec3(0.85, 0.92, 1.0), heat) * (1.0 + 5.0 * heat);
      light += shell * fade;
    }
  }
  gl_FragColor = vec4(light * smoothstep(0.0, 0.3, uIntro), 1.0);
}
`;

/** How old the remnant is at `time`, from 0 to 1; -1 before the first blast. */
function age(time: number): number {
  const cycle = (time - DELAY) / CYCLE;
  return cycle < 0 ? -1 : cycle - Math.floor(cycle);
}

export const supernova: Scene = {
  vertex: VERTEX,
  glow: GLOW,
  extent: 2.2,
  stride: STRIDE,
  layout: [
    ["aPos", 3, 0],
    ["aColor", 4, 3],
    ["aMisc", 4, 7],
    ["aMove", 2, 11],
  ],
  rest: REST_VIEW,
  introSeconds: DELAY,
  // The remnant half-way through its life.
  stillTime: DELAY + CYCLE * 0.5,
  make: makeSupernova,
  frame: (time, _intro, _still, backdrop) => ({
    uSpin: SPIN_SPEED * time,
    // The giant swells a little just before it goes.
    // Behind the console's pages nothing explodes: the remnant stays as it is half-way through
    // its life, and only churns and turns.
    uStar: backdrop ? 0 : time < DELAY ? 1 + 0.5 * (time / DELAY) ** 4 : 0,
    uAge: backdrop ? 0.5 : age(time),
    // The gas churns slowly; kept small for the light's shader.
    uChurn: (time * 0.012) % 64,
  }),
};
