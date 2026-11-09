import { SKY, STAR_HEAD, skyStars, type Scene } from "./engine";

/**
 * An aurora over the limb of a planet, seen from orbit (see engine.tsx for how a sky is drawn).
 *
 * The night side of a planet fills the bottom of the screen, its edge a shallow arc with the thin
 * blue line of its atmosphere and a fainter green line of airglow above it. To one side the sun
 * is about to rise behind it. Over the edge and in front of it hang three curtains of aurora, the
 * far one small and the near one large: each a meandering ribbon with a bright lower hem, made of
 * fine upright rays that shimmer and drift sideways, green below, turning teal and then violet
 * towards the top. The curtains brighten and fade along their length. The view does not lean
 * towards the mouse: a horizon that tips looks wrong.
 *
 * The planet and the aurora are painted by the light's shader, flat on the screen; the only stars
 * are those of the sky.
 */

/** The planet's centre, from the centre of the scene, and its radius, in scene radii: far below
 * the screen, so that only a shallow arc of its edge shows. */
const PLANET = [0.4, -5.4];
const PLANET_RADIUS = 5;
/** Where along the edge the sun is about to rise: the angle from the top of the planet (radians),
 * towards the left, away from the sign-in panel. */
const SUNRISE = -0.3;

/** Floats per star: direction (3), colour and alpha, size, twinkle, phase, kind. */
const STRIDE = 11;

function makeAurora(_count: number, sky: number): Float32Array {
  const data: number[] = [];
  // More of them than behind the other skies, since there is nothing else up there.
  for (const { pos, color, alpha, size } of skyStars(Math.round(sky * 1.6))) {
    data.push(...pos, ...color, alpha, size, 0, 0, SKY);
  }
  return new Float32Array(data);
}

/** A pixel's place from the planet's centre, in both shaders. */
const FROM_PLANET = `
vec2 fromPlanet(vec2 at) {
  return at - vec2(${PLANET[0].toFixed(2)}, ${PLANET[1].toFixed(2)});
}
`;

const VERTEX = `
${STAR_HEAD}
${FROM_PLANET}
void main() {
  vec2 px;
  if (!skyStar(px)) {
    hide();
    return;
  }
  // The planet hides the stars behind it, and its air dims those just above its edge.
  float height = length(fromPlanet((px - uCenter) / uScale)) - ${PLANET_RADIUS.toFixed(1)};
  if (height < 0.004) {
    hide();
    return;
  }
  star(px, aMisc.x * uPx, aColor.a * (0.3 + 0.7 * smoothstep(0.0, 0.1, height)) * smoothstep(0.0, 0.3, uIntro), aColor.rgb);
}
`;

const GLOW = `
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform float uIntro;
uniform float uFlow;
varying vec2 vPos;
${FROM_PLANET}
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

/** The light of one curtain at the angle 'along' the planet's edge and 'height' above it. Its
 * lower hem lies around the height 'base'; 'scale' is how near it is, and so how large. */
vec3 curtain(float along, float height, float base, float scale, float seed) {
  // Nothing of it reaches this high, or this far below its hem.
  float reach = height - base;
  if (reach > 0.75 * scale || reach < -0.1 * scale) return vec3(0.0);
  float fine = along / scale;
  // The hem meanders, in wide bends and small ones, and slowly changes its course.
  float hem = base + scale * (0.09 * (noise(vec3(fine * 9.0 + seed, uFlow * 0.05, seed)) - 0.5) + 0.03 * (noise(vec3(fine * 31.0, uFlow * 0.08, seed + 3.0)) - 0.5));
  // 0 at the hem, 1 one curtain height above it.
  float up = (height - hem) / (0.16 * scale);
  // Fine upright rays that shimmer, in broader folds that drift sideways.
  float rays = 0.3 + 0.7 * smoothstep(0.2, 0.85, noise(vec3(fine * 240.0 + seed * 7.0, uFlow * 0.5, seed)));
  rays *= 0.35 + 0.65 * noise(vec3(fine * 60.0 - uFlow * 0.15, uFlow * 0.2, seed + 9.0));
  // Only parts of its length are lit at a time.
  float lit = smoothstep(0.33, 0.8, noise(vec3(along * 4.0 - uFlow * 0.03 + seed, uFlow * 0.04, seed + 5.0)));
  // Brightest in the hem, fading with height.
  float hemline = up / 0.07;
  float body = smoothstep(-0.05, 0.06, up) * exp(-max(up, 0.0) * 1.1) + 0.7 * exp(-hemline * hemline);
  vec3 color = mix(vec3(0.15, 1.0, 0.45), vec3(0.1, 0.8, 0.75), smoothstep(0.2, 0.8, up));
  color = mix(color, vec3(0.9, 0.3, 1.3), smoothstep(0.7, 1.9, up));
  return color * body * rays * lit;
}

void main() {
  vec2 d = fromPlanet(vPos);
  float height = length(d) - ${PLANET_RADIUS.toFixed(1)};
  float along = atan(d.x, d.y);
  float above = max(height, 0.0);
  float below = max(-height, 0.0);

  // The air: a thin blue line along the edge, lit up where the sun is about to rise, with a
  // white-hot rim there and a soft glow around the spot.
  float dawn = (along - (${SUNRISE.toFixed(2)})) / 0.17;
  dawn = exp(-dawn * dawn);
  vec3 light = vec3(0.25, 0.55, 1.0) * exp(-above / 0.014) * exp(-below / 0.05) * (0.3 + 2.2 * dawn);
  light += vec3(1.0, 0.86, 0.7) * dawn * exp(-above / 0.006) * exp(-below / 0.012) * 1.6;
  vec2 sun = ${PLANET_RADIUS.toFixed(1)} * vec2(sin(${SUNRISE.toFixed(2)}), cos(${SUNRISE.toFixed(2)}));
  light += vec3(0.4, 0.6, 1.0) * 0.22 * exp(-length(d - sun) / 0.28) * step(0.0, height);
  // The airglow: a faint green line a little higher up.
  float glow = (height - 0.035) / 0.007;
  light += vec3(0.3, 0.9, 0.4) * 0.1 * exp(-glow * glow);
  // The night side: almost black, with a hint of cloud.
  if (height < 0.0) light += vec3(0.012, 0.02, 0.04) * (0.5 + noise(vec3(d * 2.2, 3.0)) + 0.5 * noise(vec3(d * 6.0, 7.0)));

  // The aurora comes in from the left on the sign-in screen.
  float on = smoothstep(0.0, 0.6, uIntro * 1.6 - (along + 0.45));
  vec3 aurora = 0.75 * curtain(along, height, 0.02, 0.8, 1.0) + curtain(along, height, -0.08, 1.15, 2.0) + 0.85 * curtain(along, height, -0.22, 1.7, 3.0);
  light += aurora * on * 1.3;
  // Its light on the ground below.
  if (height < 0.0) light += vec3(0.05, 0.3, 0.14) * on * 0.1 * exp(-below / 0.35);

  gl_FragColor = vec4((1.0 - exp(-light * 1.3)) * smoothstep(0.0, 0.3, uIntro), 1.0);
}
`;

export const aurora: Scene = {
  vertex: VERTEX,
  glow: GLOW,
  // The whole screen.
  extent: 6,
  stride: STRIDE,
  layout: [
    ["aPos", 3, 0],
    ["aColor", 4, 3],
    ["aMisc", 4, 7],
  ],
  rest: [1, 0, 0, 0, 1, 0, 0, 0, 1],
  introSeconds: 4,
  fixed: true,
  // The curtains cost far more to draw than stars do: 30 frames a second are plenty.
  gap: 30,
  stillTime: 20,
  make: makeAurora,
  // The time the curtains move by; kept small for the light's shader.
  frame: (time) => ({ uFlow: time % 4096 }),
};
