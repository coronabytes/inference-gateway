import { useEffect, useRef } from "react";
import { tilt } from "./tilt";

/**
 * A three-dimensional spiral galaxy drawn with WebGL, for the sign-in screen.
 *
 * Two logarithmic arms wind out of a bright core: a fine dust of blue and orange stars along each
 * arm, strung with beads of bright young stars, a few giants with diffraction spikes, and a faint
 * scatter between the arms. The arm stars stream inwards along their arm and vanish into the core,
 * while new ones come in at the tips, so the arms keep their shape as they flow. The core is a
 * swarm of stars on tilted orbits, the inner ones fastest, and its glow breathes. The galaxy is not
 * flat: each arm is three strands braided around its centre line, and the disc is warped. Every
 * star is a point in space, so the disc leans slightly towards the mouse (see tilt.ts). The galaxy
 * also turns slowly on its own axis. On the sign-in screen it first forms: a sparse field of stars across the screen is
 * pulled into the centre, piles up into a ball around a brightening core, and flattens into the
 * disc. Faint stars of the sky far behind it turn with the view. With reduced motion, one still
 * frame is drawn.
 *
 * `backdrop` is the quieter version behind the console's pages: fewer stars, no collapse, about
 * 30 frames a second, and a large disc off to the side, dimmed further by CSS.
 */

type Rgb = [number, number, number];

/** Star colours. Arm dust is mostly blue with some orange; the bright beads are white-blue. */
const ICE: Rgb = [0.82, 0.91, 1];
const WHITE: Rgb = [1, 1, 1];
const BLUE: Rgb = [0.58, 0.75, 1];
const CYAN: Rgb = [0.6, 0.9, 1];
const PEACH: Rgb = [1, 0.74, 0.58];
const ORANGE: Rgb = [1, 0.5, 0.28];
const GOLD: Rgb = [1, 0.9, 0.74];

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
/** Camera distance from the centre, in galaxy radii; closer exaggerates the perspective. */
const DISTANCE = 3.5;
/** The view the galaxy rests in (a rotation matrix, column-major): tipped back
 * about 50° with a slight turn, so the disc is seen at a slant. */
const REST_VIEW = [0.9966, -0.0743, 0.0347, 0.0736, 0.6233, -0.7785, 0.0362, 0.7785, 0.6267];

/** Floats per star in the vertex buffer: position (3), start (distance, direction offset, elevation)
 * and delay, colour and alpha, size, twinkle, phase, kind, and how it moves (see Motion). */
const STRIDE = 19;
/** Star kinds, as the shader sees them. */
const PLAIN = 0;
const SPIKED = 1;
const SKY = 2;

function gauss(): number {
  // Box-Muller; good enough for scattering stars.
  return Math.sqrt(-2 * Math.log(Math.random() || 1e-9)) * Math.cos(2 * Math.PI * Math.random());
}

function pick<T>(choices: [T, number][]): T {
  let roll = Math.random() * choices.reduce((sum, [, w]) => sum + w, 0);
  for (const [value, weight] of choices) {
    roll -= weight;
    if (roll <= 0) return value;
  }
  return choices[0][0];
}

function between(lo: number, hi: number): number {
  return lo + Math.random() * (hi - lo);
}

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
      distance, gauss() * 0.25, Math.asin(Math.random() * 2 - 1) * 0.75, Math.random() * 0.1,
      ...color, alpha,
      size, twinkle, Math.random() * Math.PI * 2, kind,
      ...(motion ?? [0, 0, 0, 0]),
    );
  };
  const scatter = (): [number, number, number] => [gauss(), gauss(), gauss()];

  // The bulge: a dense, warm swarm of stars orbiting the core, with some bright ones among them.
  for (let i = 0; i < count * 0.07; i++) {
    const { pos, motion } = inOrbit(0.004 + Math.abs(gauss()) * 0.045);
    add(pos, pick([[GOLD, 6], [WHITE, 3], [PEACH, 1]]), between(0.4, 0.9), between(0.5, 1.2), 0, PLAIN, motion);
  }
  for (let i = 0; i < 40; i++) {
    const { pos, motion } = inOrbit(0.01 + Math.abs(gauss()) * 0.06);
    add(pos, pick([[WHITE, 3], [GOLD, 2], [ICE, 2]]), between(0.6, 1), between(1.1, 2.2), between(0.4, 1.4), PLAIN, motion);
  }

  // The arms: a fine dust of stars along each.
  for (let i = 0; i < count * 0.42; i++) {
    const color = pick([[CYAN, 3], [BLUE, 3], [ICE, 2], [ORANGE, 1.4], [PEACH, 0.8], [WHITE, 0.4]]);
    add(scatter(), color, between(0.35, 0.9), between(0.3, 0.8), 0, PLAIN, onArm(i % 2, 1));
  }

  // A faint scatter of old stars between the arms, in a disc that is thicker and warped like the
  // arms (see the shader).
  for (let i = 0; i < count * 0.12; i++) {
    const r = 0.05 + Math.pow(Math.random(), 0.8) * 1.0;
    const a = Math.random() * Math.PI * 2;
    const warp = 0.12 * r * r * Math.sin(a);
    add([r * Math.cos(a), r * Math.sin(a), warp + gauss() * (0.015 + 0.04 * r)], pick([[BLUE, 2], [ICE, 2], [ORANGE, 1]]), between(0.06, 0.2), between(0.35, 0.7));
  }

  // Beads of bright young stars strung along the arms, alone or in small knots that travel together.
  for (let c = 0; c < count / 34; c++) {
    const place = onArm(c % 2, 0.7);
    const [x, y, z] = scatter();
    const knot = pick([[1, 8], [2, 2], [3, 1]]);
    for (let i = 0; i < knot; i++) {
      const color = pick([[ICE, 5.5], [WHITE, 2], [BLUE, 1], [PEACH, 1], [ORANGE, 0.5]]);
      const spread = i ? 0.8 : 0;
      const member: Motion = [place[0], place[1] + gauss() * 0.008 * Math.min(i, 1), place[2], place[3]];
      add([x + gauss() * spread, y + gauss() * spread, z + gauss() * spread * 0.6], color, between(0.6, 1), pick([[between(1.2, 2), 3], [between(2, 3.4), 2]]), between(0.4, 1.8), PLAIN, member);
    }
  }

  // A few giants with diffraction spikes.
  for (let i = 0; i < Math.max(6, count / 1400); i++) {
    add(scatter(), pick([[ICE, 3], [WHITE, 2], [PEACH, 1]]), between(0.85, 1), between(2.4, 3.4), between(0.3, 0.9), SPIKED, onArm(i % 2, 0.5));
  }

  // The sky: faint stars far away in every direction, which only move when the view turns.
  for (let i = 0; i < sky; i++) {
    const u = Math.random() * 2 - 1;
    const phi = Math.random() * Math.PI * 2;
    const ring = Math.sqrt(1 - u * u);
    add([ring * Math.cos(phi), ring * Math.sin(phi), u], pick([[ICE, 3], [WHITE, 2], [PEACH, 1]]), between(0.15, 0.6), between(0.35, 0.9), 0, SKY);
  }
  return new Float32Array(data);
}

const STAR_VERTEX = `
attribute vec3 aPos;
attribute vec4 aStart;
attribute vec4 aColor;
attribute vec4 aMisc;
attribute vec4 aArm;
uniform mat3 uView;
uniform float uSpin;
uniform float uTime;
uniform float uIntro;
uniform vec2 uViewport;
uniform vec2 uCenter;
uniform float uScale;
uniform float uPx;
uniform float uFocal;
uniform float uMaxPoint;
varying vec3 vColor;
varying float vAlpha;
varying float vSpread;
varying float vSpikes;

const float PI = 3.14159265;

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
    vec3 d = uView * aPos;
    if (d.z > -0.2) {
      gl_Position = vec4(0.0, 0.0, 2.0, 1.0);
      gl_PointSize = 0.0;
      return;
    }
    px = uViewport * 0.5 + d.xy / -d.z * uFocal;
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
    vec3 q = uView * p;
    // Kept from blowing up for the odd star that starts close to the camera.
    float persp = ${DISTANCE.toFixed(1)} / max(${DISTANCE.toFixed(1)} - q.z, 1.0);
    px = uCenter + q.xy * uScale * persp;
    size *= persp;
    if (aMisc.y > 0.0) alpha *= 0.72 + 0.28 * sin(uTime * aMisc.y + aMisc.z);
  }
  // The sprite holds the star's glow (and spikes) around its core; the core is 'size' pixels wide.
  // Bright stars get a wide soft glow; faint dust barely any.
  float spread = kind > 0.5 && kind < 1.5 ? 12.0 : aMisc.x > 1.0 ? 7.0 : 3.0;
  float sprite = size * 2.0 * spread;
  if (sprite < 2.0) {
    // Too small to draw: keep the light the star gives, spread over the smallest point.
    alpha *= sprite * sprite / 4.0;
    sprite = 2.0;
  } else if (sprite > uMaxPoint) {
    // Bigger than the GPU draws points: keep the core its size and cut the glow short.
    spread *= uMaxPoint / sprite;
    sprite = uMaxPoint;
  }
  gl_Position = vec4(px / uViewport * 2.0 - 1.0, 0.0, 1.0);
  gl_PointSize = sprite;
  vColor = aColor.rgb;
  vAlpha = alpha;
  vSpread = spread;
  vSpikes = kind > 0.5 && kind < 1.5 ? 1.0 : 0.0;
}
`;

const STAR_FRAGMENT = `
precision mediump float;
varying vec3 vColor;
varying float vAlpha;
varying float vSpread;
varying float vSpikes;

void main() {
  // Distance from the centre in star radii.
  vec2 v = (gl_PointCoord - 0.5) * 2.0 * vSpread;
  float r = length(v);
  float core = exp(-r * r * 1.6);
  float halo = exp(-r * 0.75) * 0.3;
  float light = (core + halo) * (1.0 - smoothstep(vSpread * 0.7, vSpread, r));
  if (vSpikes > 0.5) {
    vec2 a = abs(v);
    vec2 fade = max(vec2(0.0), 1.0 - a / vSpread);
    light += (exp(-a.y * 3.0) * fade.x * fade.x + exp(-a.x * 3.0) * fade.y * fade.y) * 0.6;
  }
  // Bright cores burn white; the colour shows in the glow around them.
  vec3 color = mix(vColor, vec3(1.0), core * 0.55);
  gl_FragColor = vec4(color * light * vAlpha, 1.0);
}
`;

const GLOW_VERTEX = `
attribute vec2 aCorner;
uniform vec2 uViewport;
uniform vec2 uCenter;
uniform float uScale;
varying vec2 vPos;

void main() {
  vPos = aCorner * 1.2;
  gl_Position = vec4((uCenter + vPos * uScale) / uViewport * 2.0 - 1.0, 0.0, 1.0);
}
`;

const GLOW_FRAGMENT = `
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

function compile(gl: WebGLRenderingContext, vertex: string, fragment: string): WebGLProgram | null {
  const program = gl.createProgram();
  if (!program) return null;
  for (const [type, source] of [[gl.VERTEX_SHADER, vertex], [gl.FRAGMENT_SHADER, fragment]] as const) {
    const shader = gl.createShader(type);
    if (!shader) return null;
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      console.warn("galaxy shader:", gl.getShaderInfoLog(shader));
      return null;
    }
    gl.attachShader(program, shader);
  }
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    console.warn("galaxy program:", gl.getProgramInfoLog(program));
    return null;
  }
  return program;
}

function uniforms(gl: WebGLRenderingContext, program: WebGLProgram, names: string[]): Record<string, WebGLUniformLocation | null> {
  return Object.fromEntries(names.map((name) => [name, gl.getUniformLocation(program, name)]));
}

export function Galaxy({ className, backdrop = false }: { className?: string; backdrop?: boolean }) {
  const ref = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = ref.current;
    const gl = canvas?.getContext("webgl", { alpha: false, antialias: false, depth: false, powerPreference: "low-power" });
    if (!canvas || !gl) return;
    const still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

    const stars = compile(gl, STAR_VERTEX, STAR_FRAGMENT);
    const glow = compile(gl, GLOW_VERTEX, GLOW_FRAGMENT);
    if (!stars || !glow) return;
    const starUniforms = uniforms(gl, stars, ["uView", "uSpin", "uTime", "uIntro", "uViewport", "uCenter", "uScale", "uPx", "uFocal", "uMaxPoint"]);
    const glowUniforms = uniforms(gl, glow, ["uViewport", "uCenter", "uScale", "uGlow"]);
    const starBuffer = gl.createBuffer();
    const glowBuffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, glowBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const maxPoint = (gl.getParameter(gl.ALIASED_POINT_SIZE_RANGE) as Float32Array)[1];
    // Light adds up, so crowded stars and the core burn brighter, as they do on a long exposure.
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE);
    gl.clearColor(3 / 255, 5 / 255, 11 / 255, 1);

    let starCount = 0;
    let width = 0;
    let height = 0;
    let dpr = 1;
    let frame = 0;

    /** Fit the canvas to its size; returns whether anything changed. */
    const resize = () => {
      // A minimized window reports no size. Keep everything as it is until it comes back, instead
      // of tearing the canvas and the stars down and building them again.
      if (!canvas.clientWidth || !canvas.clientHeight) return false;
      if (canvas.clientWidth === width && canvas.clientHeight === height) return false;
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      width = canvas.clientWidth;
      height = canvas.clientHeight;
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
      gl.viewport(0, 0, canvas.width, canvas.height);
      // Fewer stars on small screens; the look stays the same.
      const count = Math.round(Math.min(16000, Math.max(6000, (width * height) / 110)) * (backdrop ? 0.5 : 1));
      if (Math.abs(count - starCount) > count * 0.2) {
        const data = makeGalaxy(count, backdrop ? 1600 : 2600);
        gl.bindBuffer(gl.ARRAY_BUFFER, starBuffer);
        gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
        starCount = data.length / STRIDE;
      }
      return true;
    };

    /** One frame at `time` seconds of animation. */
    const draw = (time: number) => {
      // Sign-in: left of the panel on wide screens, above it on narrow ones. Backdrop: a large
      // disc towards the lower right, partly off screen.
      const wide = width > 900;
      const cx = backdrop ? width * 0.72 : wide ? width * 0.36 : width / 2;
      const cy = backdrop ? height * 0.62 : wide ? height / 2 : height * 0.3;
      const fit = backdrop
        ? Math.max(width, height) * 0.42
        : wide
          ? Math.min(width * 0.3, height * 0.47)
          : Math.min(width * 0.46, height * 0.27);
      const radius = fit;
      // WebGL counts pixels from the bottom left.
      const center = [cx * dpr, (height - cy) * dpr];
      const viewport = [canvas.width, canvas.height];
      const intro = backdrop || still ? 2 : time / INTRO_SECONDS;

      gl.clear(gl.COLOR_BUFFER_BIT);

      gl.useProgram(glow);
      gl.uniform2fv(glowUniforms.uViewport, viewport);
      gl.uniform2fv(glowUniforms.uCenter, center);
      gl.uniform1f(glowUniforms.uScale, radius * dpr);
      // The core lights up as the stars pile into it, then breathes gently.
      const breath = still ? 1 : 1 + 0.07 * Math.sin(time * 1.1) + 0.04 * Math.sin(time * 2.9 + 1);
      gl.uniform1f(glowUniforms.uGlow, (0.1 + 0.9 * Math.min(1, Math.max(0, (intro - 0.3) / 0.5))) * breath);
      gl.bindBuffer(gl.ARRAY_BUFFER, glowBuffer);
      const corner = gl.getAttribLocation(glow, "aCorner");
      gl.enableVertexAttribArray(corner);
      gl.vertexAttribPointer(corner, 2, gl.FLOAT, false, 0, 0);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      gl.disableVertexAttribArray(corner);

      gl.useProgram(stars);
      gl.uniformMatrix3fv(starUniforms.uView, false, controls.view());
      gl.uniform1f(starUniforms.uSpin, SPIN_START + (still ? 0 : SPIN_SPEED * time));
      gl.uniform1f(starUniforms.uTime, time);
      gl.uniform1f(starUniforms.uIntro, intro);
      gl.uniform2fv(starUniforms.uViewport, viewport);
      gl.uniform2fv(starUniforms.uCenter, center);
      gl.uniform1f(starUniforms.uScale, radius * dpr);
      gl.uniform1f(starUniforms.uPx, (dpr * fit) / 380);
      gl.uniform1f(starUniforms.uFocal, Math.max(width, height) * dpr * 0.9);
      gl.uniform1f(starUniforms.uMaxPoint, maxPoint);
      gl.bindBuffer(gl.ARRAY_BUFFER, starBuffer);
      const layout: [string, number, number][] = [["aPos", 3, 0], ["aStart", 4, 3], ["aColor", 4, 7], ["aMisc", 4, 11], ["aArm", 4, 15]];
      const enabled: number[] = [];
      for (const [name, size, offset] of layout) {
        const at = gl.getAttribLocation(stars, name);
        if (at < 0) continue;
        gl.enableVertexAttribArray(at);
        gl.vertexAttribPointer(at, size, gl.FLOAT, false, STRIDE * 4, offset * 4);
        enabled.push(at);
      }
      gl.drawArrays(gl.POINTS, 0, starCount);
      for (const at of enabled) gl.disableVertexAttribArray(at);
    };

    // The disc leans towards the mouse, on the sign-in screen and behind the pages alike.
    const controls = tilt({ rest: REST_VIEW, still });

    // Animation time in seconds. It only advances while the page is shown, by at most a tenth of a
    // second per frame, so coming back after a while carries on where it left off. Behind the pages
    // it runs at half speed (spin, breathing and twinkle alike), to stay in the background; the
    // tilt towards the mouse keeps real time.
    const pace = backdrop ? 0.5 : 1;
    let clock = 0;
    let prev: number | null = null;
    let last = 0;
    const loop = (ms: number) => {
      const dt = prev === null ? 0 : Math.min(ms - prev, 100) / 1000;
      clock += dt * pace;
      prev = ms;
      controls.step(dt);
      // The backdrop moves slowly; half the frame rate is plenty and saves power.
      if (!backdrop || ms - last >= 30) {
        draw(clock);
        last = ms;
      }
      frame = requestAnimationFrame(loop);
    };
    const start = () => {
      if (still || frame) return;
      prev = null;
      frame = requestAnimationFrame(loop);
    };
    const stop = () => {
      cancelAnimationFrame(frame);
      frame = 0;
    };
    // Stop entirely while the page is hidden (minimized window, other tab), and resume on return.
    const onVisibility = () => (document.hidden ? stop() : start());

    resize();
    const observer = new ResizeObserver(() => {
      if (resize()) draw(clock);
    });
    observer.observe(canvas);
    document.addEventListener("visibilitychange", onVisibility);
    draw(0);
    if (!document.hidden) start();
    return () => {
      stop();
      observer.disconnect();
      document.removeEventListener("visibilitychange", onVisibility);
      controls.dispose();
      gl.deleteBuffer(starBuffer);
      gl.deleteBuffer(glowBuffer);
      gl.deleteProgram(stars);
      gl.deleteProgram(glow);
    };
  }, [backdrop]);

  return <canvas ref={ref} className={className} aria-hidden="true" />;
}
