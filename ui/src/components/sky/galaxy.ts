import {
  BLUE,
  CYAN,
  GOLD,
  ICE,
  ORANGE,
  PEACH,
  PLAIN,
  SKY,
  SPIKED,
  STAR_HEAD,
  WHITE,
  between,
  gauss,
  pick,
  skyStars,
  type Rgb,
  type Scene,
} from "./engine";

/**
 * A three-dimensional spiral galaxy (see engine.tsx for how a sky is drawn).
 *
 * Two logarithmic arms wind out of a bright core: a fine dust of blue and orange stars along each
 * arm, strung with beads of bright young stars, a few giants with diffraction spikes, and a faint
 * scatter between the arms. The arm stars stream inwards along their arm and vanish into the core,
 * while new ones come in at the tips, so the arms keep their shape as they flow. The core is a
 * swarm of stars on tilted orbits, the inner ones fastest, and its glow breathes. The galaxy is not
 * flat: each arm is three strands braided around its centre line, and the disc is warped. The
 * galaxy also turns slowly on its own axis. On the sign-in screen it first forms: a sparse field of
 * stars across the screen is pulled into the centre, piles up into a ball around a brightening
 * core, and flattens into the disc.
 */

/** Where each arm starts, as a fraction of the galaxy radius. */
const ARM_START = 0.07;
/** How tightly the arms wind: r = ARM_START * e^(WINDING * angle). Smaller winds them tighter. */
const WINDING = 0.26;
/** Arm stars stream inwards along their arm from its tip (ARM_OUT) into the core (ARM_IN), where
 * they vanish and start again at the tip, at FLOW galaxy radii per second: about 90 seconds. */
const ARM_IN = 0.015;
const ARM_OUT = 1.08;
const FLOW = 0.012;
/** The galaxy turns on its own axis this fast (radians per second): once in about three minutes,
 * starting from SPIN_START. */
const SPIN_SPEED = 0.035;
const SPIN_START = 1.6;
/** Seconds for the stars to collapse into the galaxy on the sign-in screen. */
const INTRO_SECONDS = 4.5;
/** The view the galaxy rests in (a rotation matrix, column-major): tipped back
 * about 50° with a slight turn, so the disc is seen at a slant. */
const REST_VIEW = [0.9966, -0.0743, 0.0347, 0.0736, 0.6233, -0.7785, 0.0362, 0.7785, 0.6267];

/** Floats per star in the vertex buffer: position (3), start (distance, direction offset, elevation)
 * and delay, colour and alpha, size, twinkle, phase, kind, and how it moves (see Motion). */
const STRIDE = 19;

/** How a star moves, besides turning with the galaxy, as the shader reads it. On an arm: the arm
 * + 1, its radius at time 0, how wide the arm is for it (dust scatters widest, giants least), and
 * which of the arm's three strands it follows (-1, 0, 1). Orbiting the core: -1, the orbit's radius
 * and angular speed. Neither: all 0. */
type Motion = [kind: number, radius: number, a: number, b: number];

function onArm(arm: number, width: number): Motion {
  // Spread evenly over the arm's length, so the stream looks the same as it flows.
  return [arm + 1, ARM_IN + Math.random() * (ARM_OUT - ARM_IN), width, Math.floor(Math.random() * 3) - 1];
}

/** A star orbiting the core at radius `r`; its position is its phase, orbit node and inclination. */
function inOrbit(r: number): { pos: [number, number, number]; motion: Motion } {
  // Mostly near the disc's plane, some steeply tilted, so the core is a swarm rather than a plate.
  // The inner ones go round fastest.
  return {
    pos: [Math.random() * Math.PI * 2, Math.random() * Math.PI * 2, gauss() * 0.7],
    motion: [-1, r, 0.04 / (r + 0.01), 0],
  };
}

/** Builds the vertex data for a galaxy of about `count` stars plus `sky` stars far behind it. */
function makeGalaxy(count: number, sky: number): Float32Array {
  const data: number[] = [];
  /** Adds a star. Unless it moves, `pos` is where it is; on an arm, it is its scatter around the
   * arm's centre line, in arm widths (along, across, up); in orbit, see inOrbit. The shader works
   * out where a moving star is. */
  const add = (pos: [number, number, number], color: Rgb, alpha: number, size: number, twinkle = 0, kind = PLAIN, motion?: Motion) => {
    // Before the collapse, each star sits out in a wide, sparse field that fills the screen, roughly
    // in the direction it will end up in: its distance from the centre, how far its direction is
    // off, its elevation above the disc, and a short random wait.
    const r = motion ? motion[1] : Math.hypot(pos[0], pos[1]);
    const distance = Math.max(r + 0.3, 0.6 + 2.6 * Math.sqrt(Math.random()));
    data.push(
      ...pos,
      distance,
      gauss() * 0.25,
      Math.asin(Math.random() * 2 - 1) * 0.75,
      Math.random() * 0.1,
      ...color,
      alpha,
      size,
      twinkle,
      Math.random() * Math.PI * 2,
      kind,
      ...(motion ?? [0, 0, 0, 0]),
    );
  };
  const scatter = (): [number, number, number] => [gauss(), gauss(), gauss()];

  // The bulge: a dense, warm swarm of stars orbiting the core, with some bright ones among them.
  for (let i = 0; i < count * 0.07; i++) {
    const { pos, motion } = inOrbit(0.004 + Math.abs(gauss()) * 0.045);
    add(
      pos,
      pick([
        [GOLD, 6],
        [WHITE, 3],
        [PEACH, 1],
      ]),
      between(0.4, 0.9),
      between(0.5, 1.2),
      0,
      PLAIN,
      motion,
    );
  }
  for (let i = 0; i < 40; i++) {
    const { pos, motion } = inOrbit(0.01 + Math.abs(gauss()) * 0.06);
    add(
      pos,
      pick([
        [WHITE, 3],
        [GOLD, 2],
        [ICE, 2],
      ]),
      between(0.6, 1),
      between(1.1, 2.2),
      between(0.4, 1.4),
      PLAIN,
      motion,
    );
  }

  // The arms: a fine dust of stars along each.
  for (let i = 0; i < count * 0.42; i++) {
    const color = pick([
      [CYAN, 3],
      [BLUE, 3],
      [ICE, 2],
      [ORANGE, 1.4],
      [PEACH, 0.8],
      [WHITE, 0.4],
    ]);
    add(scatter(), color, between(0.35, 0.9), between(0.3, 0.8), 0, PLAIN, onArm(i % 2, 1));
  }

  // A faint scatter of old stars between the arms, in a disc that is thicker and warped like the
  // arms (see the shader).
  for (let i = 0; i < count * 0.12; i++) {
    const r = 0.05 + Math.random() ** 0.8 * 1.0;
    const a = Math.random() * Math.PI * 2;
    const warp = 0.12 * r * r * Math.sin(a);
    add(
      [r * Math.cos(a), r * Math.sin(a), warp + gauss() * (0.015 + 0.04 * r)],
      pick([
        [BLUE, 2],
        [ICE, 2],
        [ORANGE, 1],
      ]),
      between(0.06, 0.2),
      between(0.35, 0.7),
    );
  }

  // Beads of bright young stars strung along the arms, alone or in small knots that travel together.
  for (let c = 0; c < count / 34; c++) {
    const place = onArm(c % 2, 0.7);
    const [x, y, z] = scatter();
    const knot = pick([
      [1, 8],
      [2, 2],
      [3, 1],
    ]);
    for (let i = 0; i < knot; i++) {
      const color = pick([
        [ICE, 5.5],
        [WHITE, 2],
        [BLUE, 1],
        [PEACH, 1],
        [ORANGE, 0.5],
      ]);
      const spread = i ? 0.8 : 0;
      const member: Motion = [place[0], place[1] + gauss() * 0.008 * Math.min(i, 1), place[2], place[3]];
      add(
        [x + gauss() * spread, y + gauss() * spread, z + gauss() * spread * 0.6],
        color,
        between(0.6, 1),
        pick([
          [between(1.2, 2), 3],
          [between(2, 3.4), 2],
        ]),
        between(0.4, 1.8),
        PLAIN,
        member,
      );
    }
  }

  // A few giants with diffraction spikes.
  for (let i = 0; i < Math.max(6, count / 1400); i++) {
    add(
      scatter(),
      pick([
        [ICE, 3],
        [WHITE, 2],
        [PEACH, 1],
      ]),
      between(0.85, 1),
      between(2.4, 3.4),
      between(0.3, 0.9),
      SPIKED,
      onArm(i % 2, 0.5),
    );
  }

  for (const { pos, color, alpha, size } of skyStars(sky)) add(pos, color, alpha, size, 0, SKY);
  return new Float32Array(data);
}

const VERTEX = `
${STAR_HEAD}
attribute vec4 aStart;
attribute vec4 aArm;
uniform float uSpin;

/** Where an arm star is now. The arms are logarithmic spirals that wind outwards clockwise as seen
 * from the front, with a slight wobble so they look grown rather than drawn. Each star streams in
 * along its arm, winding faster the closer it gets, and starts again at the tip once the core has
 * swallowed it. Arms are not flat: each is three strands braided around its centre line, it rises
 * and dips along its length, and the whole disc is warped into a shallow S. */
vec3 armStar(inout float alpha) {
  float arm = aArm.x - 1.0;
  float span = ${(ARM_OUT - ARM_IN).toFixed(3)};
  float r = ${ARM_IN.toFixed(3)} + mod(aArm.y - ${ARM_IN.toFixed(3)} - ${FLOW.toFixed(4)} * uTime, span);
  float angle = arm * PI - log(r / ${ARM_START.toFixed(3)}) / ${WINDING.toFixed(3)} + 0.12 * sin(r * 7.0 + arm * 2.0);
  vec2 out_ = vec2(cos(angle), sin(angle));
  vec2 side = vec2(-out_.y, out_.x);
  float width = aArm.z * (0.01 + 0.022 * r);
  float twist = angle * 1.3 + arm * 2.0;
  float strand = aArm.w * (0.012 + 0.035 * r);
  float lift = 0.07 * sin(angle * 0.45 + arm * 1.9) * smoothstep(0.08, 0.5, r) + 0.12 * r * r * out_.y;
  vec2 flat_ = r * out_ + side * (aPos.x * width + strand * cos(twist)) + out_ * aPos.y * width;
  // Fade in at the tip and out as the star falls into the core.
  alpha *= smoothstep(${ARM_IN.toFixed(3)}, ${ARM_IN.toFixed(3)} + 0.035, r) * (1.0 - smoothstep(0.82, ${ARM_OUT.toFixed(3)}, r));
  return vec3(flat_, aPos.z * width * 0.9 + strand * sin(twist) * 1.4 + lift);
}

/** Where a core star is now on its tilted circular orbit. */
vec3 orbitStar() {
  float angle = aPos.x + aArm.z * uTime;
  vec3 p = aArm.y * vec3(cos(angle), sin(angle), 0.0);
  p = vec3(p.x, p.y * cos(aPos.z), p.y * sin(aPos.z));
  float c = cos(aPos.y);
  float s = sin(aPos.y);
  return vec3(c * p.x - s * p.y, s * p.x + c * p.y, p.z);
}

void main() {
  float kind = aMisc.w;
  float size = aMisc.x * uPx;
  float alpha = aColor.a;
  vec2 px;
  if (kind > 1.5) {
    // A sky star, infinitely far away: only its direction matters. Those behind the camera go.
    if (!skyStar(px)) {
      hide();
      return;
    }
  } else {
    // The collapse: each star is pulled straight in towards the centre, slowly at first and faster
    // as it falls, the far ones taking longest. They pile up into a round ball around the core,
    // which then flattens into the disc as each star drops onto its place in the arms.
    float falling = 0.3 + 0.12 * aStart.x;
    float fall = clamp((uIntro - aStart.w) / falling, 0.0, 1.0);
    // p^4 (5 - 4p): a barely moving start, speeding up hard until three quarters of the way, and
    // only a short braking as the star is caught by the core.
    fall = fall * fall * fall * fall * (5.0 - 4.0 * fall);
    float settle = smoothstep(0.0, 1.0, (uIntro - aStart.w - falling * 0.8) / 0.3);
    vec3 target = aArm.x > 0.5 ? armStar(alpha) : aArm.x < -0.5 ? orbitStar() : aPos;
    float reach = mix(aStart.x, length(target.xy), fall);
    float bearing = atan(target.y, target.x) + aStart.y * (1.0 - fall);
    float elevation = aStart.z * (1.0 - settle);
    vec3 p = vec3(reach * cos(elevation) * vec2(cos(bearing), sin(bearing)), reach * sin(elevation) + target.z * settle);
    // Stars fade in from nothing rather than popping up.
    alpha *= smoothstep(0.0, 0.12, uIntro);
    float c = cos(uSpin);
    float s = sin(uSpin);
    p.xy = vec2(c * p.x - s * p.y, s * p.x + c * p.y);
    px = project(p, size);
    alpha *= twinkle();
  }
  star(px, size, alpha, aColor.rgb);
}
`;

const GLOW = `
precision mediump float;
uniform float uGlow;
varying vec2 vPos;

void main() {
  // The core's light: a hot centre, a soft halo, and a faint wide haze.
  float r = length(vPos);
  float light = 1.2 * exp(-pow(r / 0.03, 2.0)) + 0.55 * exp(-r / 0.07) + 0.12 * exp(-r / 0.3);
  light *= 1.0 - smoothstep(0.8, 1.2, r);
  gl_FragColor = vec4(vec3(1.0, 0.93, 0.84) * light * uGlow, 1.0);
}
`;

export const galaxy: Scene = {
  vertex: VERTEX,
  glow: GLOW,
  extent: 1.2,
  stride: STRIDE,
  layout: [
    ["aPos", 3, 0],
    ["aStart", 4, 3],
    ["aColor", 4, 7],
    ["aMisc", 4, 11],
    ["aArm", 4, 15],
  ],
  rest: REST_VIEW,
  introSeconds: INTRO_SECONDS,
  make: makeGalaxy,
  frame: (time, intro, still) => ({
    uSpin: SPIN_START + (still ? 0 : SPIN_SPEED * time),
    // The core lights up as the stars pile into it, then breathes gently.
    uGlow:
      (0.1 + 0.9 * Math.min(1, Math.max(0, (intro - 0.3) / 0.5))) *
      (still ? 1 : 1 + 0.07 * Math.sin(time * 1.1) + 0.04 * Math.sin(time * 2.9 + 1)),
  }),
};
