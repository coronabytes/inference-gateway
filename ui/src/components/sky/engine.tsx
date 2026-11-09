import { useEffect, useRef } from "react";
import { tilt, type Mat3 } from "./tilt";

/**
 * What the skies (galaxy.ts, pulsar.ts, supernova.ts, aurora.ts) share: the canvas, the frame loop and the two
 * WebGL passes every scene is drawn with.
 *
 * A scene is a cloud of stars, each a point in space drawn as a small glowing sprite, over a quad of
 * soft light around the centre (a core's glow, a flash). Light adds up, so crowded stars burn
 * brighter, as they do on a long exposure. The scene leans slightly towards the mouse (see tilt.ts),
 * and faint stars of the sky far behind it turn with the view. With reduced motion, one still frame
 * is drawn.
 *
 * `backdrop` is the quieter version behind the console's pages: fewer stars, no intro, about
 * 30 frames a second, at half speed, and a large scene off to the side, dimmed further by CSS.
 */

export type Rgb = [number, number, number];

export const ICE: Rgb = [0.82, 0.91, 1];
export const WHITE: Rgb = [1, 1, 1];
export const BLUE: Rgb = [0.58, 0.75, 1];
export const CYAN: Rgb = [0.6, 0.9, 1];
export const PEACH: Rgb = [1, 0.74, 0.58];
export const ORANGE: Rgb = [1, 0.5, 0.28];
export const GOLD: Rgb = [1, 0.9, 0.74];
export const VIOLET: Rgb = [0.72, 0.6, 1];
export const RED: Rgb = [1, 0.36, 0.3];
export const TEAL: Rgb = [0.4, 0.92, 0.86];
export const PINK: Rgb = [1, 0.56, 0.72];

/** Star kinds, as the shaders see them (aMisc.w). */
export const PLAIN = 0;
export const SPIKED = 1;
export const SKY = 2;

/** Camera distance from the centre, in scene radii; closer exaggerates the perspective. */
export const DISTANCE = 3.5;

export function gauss(): number {
  // Box-Muller; good enough for scattering stars.
  return Math.sqrt(-2 * Math.log(Math.random() || 1e-9)) * Math.cos(2 * Math.PI * Math.random());
}

export function pick<T>(choices: [T, number][]): T {
  let roll = Math.random() * choices.reduce((sum, [, w]) => sum + w, 0);
  for (const [value, weight] of choices) {
    roll -= weight;
    if (roll <= 0) return value;
  }
  return choices[0][0];
}

export function between(lo: number, hi: number): number {
  return lo + Math.random() * (hi - lo);
}

/** A random direction, evenly over the sphere. */
export function direction(): [number, number, number] {
  const u = Math.random() * 2 - 1;
  const phi = Math.random() * Math.PI * 2;
  const ring = Math.sqrt(1 - u * u);
  return [ring * Math.cos(phi), ring * Math.sin(phi), u];
}

/** The sky: `count` faint stars far away in every direction, which only move when the view turns. */
export function skyStars(count: number): { pos: [number, number, number]; color: Rgb; alpha: number; size: number }[] {
  return Array.from({ length: count }, () => ({
    pos: direction(),
    color: pick([
      [ICE, 3],
      [WHITE, 2],
      [PEACH, 1],
    ]),
    alpha: between(0.15, 0.6),
    size: between(0.35, 0.9),
  }));
}

export interface Scene {
  /** Vertex shader of the stars: STAR_HEAD, then the scene's own attributes and `main`. */
  vertex: string;
  /** Fragment shader of the light around the centre; `vPos` is the pixel's place in scene radii.
   * A scene may paint everything there and have no stars at all. */
  glow: string;
  /** How far the light reaches from the centre, in scene radii. */
  extent: number;
  /** Floats per star in the vertex buffer, and where each attribute sits: name, size, offset. */
  stride: number;
  layout: [name: string, size: number, offset: number][];
  /** The view the scene rests in. */
  rest: Mat3;
  /** Seconds until `uIntro` reaches 1 on the sign-in screen. */
  introSeconds: number;
  /** Milliseconds between frames at least, for a scene that is costly to draw; every frame without. */
  gap?: number;
  /** How strongly bright parts glow into what is around them (see makeBloom); none without. */
  bloom?: number;
  /** The view stays as it is, instead of leaning towards the mouse. */
  fixed?: boolean;
  /** The time of the one frame drawn with reduced motion. */
  stillTime?: number;
  /** Builds the vertex data: about `count` stars plus `sky` stars far behind them. */
  make(count: number, sky: number): Float32Array;
  /** The scene's own uniforms for a frame, set on both passes. `backdrop` is true behind the
   * console's pages, where a scene should keep calm: nothing that flashes or goes off. */
  frame?(time: number, intro: number, still: boolean, backdrop: boolean): Record<string, number>;
}

/**
 * The start of every star vertex shader. `aPos` is a star's place (a sky star's direction),
 * `aColor` its colour and alpha, `aMisc` its size, twinkle speed, twinkle phase and kind.
 */
export const STAR_HEAD = `
attribute vec3 aPos;
attribute vec4 aColor;
attribute vec4 aMisc;
uniform mat3 uView;
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

/** Draws nothing for this star. */
void hide() {
  gl_Position = vec4(0.0, 0.0, 2.0, 1.0);
  gl_PointSize = 0.0;
}

/** Where a sky star is on screen. It is infinitely far away, so only its direction matters; false
 * for one behind the camera. */
bool skyStar(out vec2 px) {
  vec3 d = uView * aPos;
  if (d.z > -0.2) return false;
  px = uViewport * 0.5 + d.xy / -d.z * uFocal;
  return true;
}

/** Where the point 'p' of the scene is on screen; nearer stars are drawn larger. */
vec2 project(vec3 p, inout float size) {
  vec3 q = uView * p;
  // Kept from blowing up for the odd star close to the camera.
  float persp = ${DISTANCE.toFixed(1)} / max(${DISTANCE.toFixed(1)} - q.z, 1.0);
  size *= persp;
  return uCenter + q.xy * uScale * persp;
}

float twinkle() {
  return aMisc.y > 0.0 ? 0.72 + 0.28 * sin(uTime * aMisc.y + aMisc.z) : 1.0;
}

/** Draws the star at 'px' (pixels), its core 'size' pixels wide. */
void star(vec2 px, float size, float alpha, vec3 color) {
  float kind = aMisc.w;
  // The sprite holds the star's glow (and spikes) around its core. Bright stars get a wide soft
  // glow; faint dust barely any.
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
  vColor = color;
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
uniform float uExtent;
varying vec2 vPos;

void main() {
  vPos = aCorner * uExtent;
  gl_Position = vec4((uCenter + vPos * uScale) / uViewport * 2.0 - 1.0, 0.0, 1.0);
}
`;

interface Program {
  program: WebGLProgram;
  /** A uniform's location; null (which WebGL ignores) for one the shader does not use. */
  at(name: string): WebGLUniformLocation | null;
}

function compile(gl: WebGLRenderingContext, vertex: string, fragment: string): Program | null {
  const program = gl.createProgram();
  if (!program) return null;
  for (const [type, source] of [
    [gl.VERTEX_SHADER, vertex],
    [gl.FRAGMENT_SHADER, fragment],
  ] as const) {
    const shader = gl.createShader(type);
    if (!shader) return null;
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      console.warn("sky shader:", gl.getShaderInfoLog(shader));
      return null;
    }
    gl.attachShader(program, shader);
  }
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    console.warn("sky program:", gl.getProgramInfoLog(program));
    return null;
  }
  const found = new Map<string, WebGLUniformLocation | null>();
  return {
    program,
    at: (name) => {
      if (!found.has(name)) found.set(name, gl.getUniformLocation(program, name));
      return found.get(name) ?? null;
    },
  };
}

const POST_VERTEX = `
attribute vec2 aCorner;
varying vec2 vAt;

void main() {
  vAt = aCorner * 0.5 + 0.5;
  gl_Position = vec4(aCorner, 0.0, 1.0);
}
`;

/** The bright parts of the picture, at a quarter of its size. */
const BLOOM_BRIGHT = `
precision mediump float;
uniform sampler2D uImage;
uniform vec2 uStep;
varying vec2 vAt;

void main() {
  vec3 c = 0.25 * (
    texture2D(uImage, vAt + uStep * vec2(-1.0, -1.0)).rgb + texture2D(uImage, vAt + uStep * vec2(1.0, -1.0)).rgb +
    texture2D(uImage, vAt + uStep * vec2(-1.0, 1.0)).rgb + texture2D(uImage, vAt + uStep * vec2(1.0, 1.0)).rgb
  );
  gl_FragColor = vec4(c * smoothstep(0.25, 0.85, max(c.r, max(c.g, c.b))), 1.0);
}
`;

/** A blur along one axis: nine samples, 'uStep' apart, weighted as a bell curve. */
const BLOOM_BLUR = `
precision mediump float;
uniform sampler2D uImage;
uniform vec2 uStep;
varying vec2 vAt;

void main() {
  vec3 c = texture2D(uImage, vAt).rgb * 0.227;
  c += (texture2D(uImage, vAt + uStep).rgb + texture2D(uImage, vAt - uStep).rgb) * 0.1946;
  c += (texture2D(uImage, vAt + uStep * 2.0).rgb + texture2D(uImage, vAt - uStep * 2.0).rgb) * 0.1216;
  c += (texture2D(uImage, vAt + uStep * 3.0).rgb + texture2D(uImage, vAt - uStep * 3.0).rgb) * 0.0541;
  c += (texture2D(uImage, vAt + uStep * 4.0).rgb + texture2D(uImage, vAt - uStep * 4.0).rgb) * 0.0162;
  gl_FragColor = vec4(c, 1.0);
}
`;

/** The picture with its glow laid over it, as light on light: neither burns the other out. */
const BLOOM_COMPOSITE = `
precision mediump float;
uniform sampler2D uImage;
uniform sampler2D uBloom;
uniform float uStrength;
varying vec2 vAt;

void main() {
  vec3 c = texture2D(uImage, vAt).rgb;
  vec3 glow = min(texture2D(uBloom, vAt).rgb * uStrength, 1.0);
  gl_FragColor = vec4(1.0 - (1.0 - c) * (1.0 - glow), 1.0);
}
`;

interface Bloom {
  /** Fit to a canvas of this many pixels. */
  resize(width: number, height: number): void;
  /** What is drawn from here on goes into a picture of its own instead of onto the canvas. */
  begin(): void;
  /** Puts that picture on the canvas, with its bright parts glowing by `strength`. */
  end(strength: number): void;
  dispose(): void;
}

/**
 * Bloom, for a scene that asks for it: the bright parts of the picture glow softly into what is
 * around them, as they do through a lens. The scene is drawn into a texture; its bright parts are
 * copied at a quarter of the size and blurred, wider each round; and the two are put on the canvas
 * together. `quad` holds the corners of a square that covers the canvas.
 */
function makeBloom(gl: WebGLRenderingContext, quad: WebGLBuffer | null): Bloom | null {
  const bright = compile(gl, POST_VERTEX, BLOOM_BRIGHT);
  const blur = compile(gl, POST_VERTEX, BLOOM_BLUR);
  const composite = compile(gl, POST_VERTEX, BLOOM_COMPOSITE);
  if (!bright || !blur || !composite) return null;
  /** A texture to draw into. */
  const target = () => {
    const texture = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    const buffer = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, buffer);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, texture, 0);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    return { texture, buffer };
  };
  const picture = target();
  // The two small pictures the blur goes back and forth between.
  const small = [target(), target()];
  let full = [1, 1];
  let quarter = [1, 1];

  /** Draws `from` through `program` into `to` (the canvas when null), `size` pixels large. */
  const pass = (program: Program, from: WebGLTexture | null, to: WebGLFramebuffer | null, size: number[], step: number[]) => {
    gl.bindFramebuffer(gl.FRAMEBUFFER, to);
    gl.viewport(0, 0, size[0], size[1]);
    gl.useProgram(program.program);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, from);
    gl.uniform1i(program.at("uImage"), 0);
    gl.uniform2fv(program.at("uStep"), step);
    const corner = gl.getAttribLocation(program.program, "aCorner");
    gl.enableVertexAttribArray(corner);
    gl.vertexAttribPointer(corner, 2, gl.FLOAT, false, 0, 0);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    gl.disableVertexAttribArray(corner);
  };

  return {
    resize: (width, height) => {
      full = [width, height];
      quarter = [Math.max(1, Math.round(width / 4)), Math.max(1, Math.round(height / 4))];
      const sized: [WebGLTexture | null, number[]][] = [
        [picture.texture, full],
        [small[0].texture, quarter],
        [small[1].texture, quarter],
      ];
      for (const [texture, size] of sized) {
        gl.bindTexture(gl.TEXTURE_2D, texture);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, size[0], size[1], 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
      }
    },
    begin: () => {
      gl.bindFramebuffer(gl.FRAMEBUFFER, picture.buffer);
      gl.viewport(0, 0, full[0], full[1]);
    },
    end: (strength) => {
      // Each pass replaces what it draws on, rather than adding to it.
      gl.disable(gl.BLEND);
      gl.bindBuffer(gl.ARRAY_BUFFER, quad);
      pass(bright, picture.texture, small[0].buffer, quarter, [1 / full[0], 1 / full[1]]);
      for (const reach of [1, 2, 3.5, 6]) {
        pass(blur, small[0].texture, small[1].buffer, quarter, [reach / quarter[0], 0]);
        pass(blur, small[1].texture, small[0].buffer, quarter, [0, reach / quarter[1]]);
      }
      gl.useProgram(composite.program);
      gl.activeTexture(gl.TEXTURE1);
      gl.bindTexture(gl.TEXTURE_2D, small[0].texture);
      gl.uniform1i(composite.at("uBloom"), 1);
      gl.uniform1f(composite.at("uStrength"), strength);
      pass(composite, picture.texture, null, full, [0, 0]);
      gl.enable(gl.BLEND);
    },
    dispose: () => {
      for (const { texture, buffer } of [picture, ...small]) {
        gl.deleteTexture(texture);
        gl.deleteFramebuffer(buffer);
      }
      for (const { program } of [bright, blur, composite]) gl.deleteProgram(program);
    },
  };
}

export function SkyCanvas({ scene, className, backdrop = false }: { scene: Scene; className?: string; backdrop?: boolean }) {
  const ref = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = ref.current;
    const gl = canvas?.getContext("webgl", { alpha: false, antialias: false, depth: false, powerPreference: "low-power" });
    if (!canvas || !gl) return;
    const still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

    const stars = compile(gl, scene.vertex, STAR_FRAGMENT);
    const glow = compile(gl, GLOW_VERTEX, scene.glow);
    if (!stars || !glow) return;
    const starBuffer = gl.createBuffer();
    const glowBuffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, glowBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const maxPoint = (gl.getParameter(gl.ALIASED_POINT_SIZE_RANGE) as Float32Array)[1];
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE);
    gl.clearColor(3 / 255, 5 / 255, 11 / 255, 1);
    const bloom = scene.bloom ? makeBloom(gl, glowBuffer) : null;

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
      // Behind the pages, a scene that is costly to draw gets no more than one pixel per CSS
      // pixel: it is dimmed and half hidden there, and should not slow the console down.
      dpr = Math.min(window.devicePixelRatio || 1, backdrop && scene.gap ? 1 : 2);
      width = canvas.clientWidth;
      height = canvas.clientHeight;
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
      gl.viewport(0, 0, canvas.width, canvas.height);
      bloom?.resize(canvas.width, canvas.height);
      // Fewer stars on small screens; the look stays the same.
      const count = Math.round(Math.min(16000, Math.max(6000, (width * height) / 110)) * (backdrop ? 0.5 : 1));
      if (Math.abs(count - starCount) > count * 0.2) {
        const data = scene.make(count, backdrop ? 1600 : 2600);
        gl.bindBuffer(gl.ARRAY_BUFFER, starBuffer);
        gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
        starCount = data.length / scene.stride;
      }
      return true;
    };

    /** One frame at `time` seconds of animation. */
    const draw = (time: number) => {
      // Sign-in: left of the panel on wide screens, above it on narrow ones. Backdrop: a large
      // scene towards the lower right, partly off screen.
      const wide = width > 900;
      const cx = backdrop ? width * 0.72 : wide ? width * 0.36 : width / 2;
      const cy = backdrop ? height * 0.62 : wide ? height / 2 : height * 0.3;
      const radius = backdrop
        ? Math.max(width, height) * 0.42
        : wide
          ? Math.min(width * 0.3, height * 0.47)
          : Math.min(width * 0.46, height * 0.27);
      // WebGL counts pixels from the bottom left.
      const center = [cx * dpr, (height - cy) * dpr];
      const viewport = [canvas.width, canvas.height];
      const intro = backdrop || still ? 2 : time / scene.introSeconds;
      const own = scene.frame?.(time, intro, still, backdrop) ?? {};
      const view = controls.view();

      const use = (p: Program) => {
        gl.useProgram(p.program);
        gl.uniformMatrix3fv(p.at("uView"), false, view);
        gl.uniform1f(p.at("uTime"), time);
        // Where the sky's stars are drawn, a direction 45° off the view's axis is this many scene
        // radii from the middle of the screen.
        gl.uniform1f(p.at("uZoom"), (Math.max(width, height) * 0.9) / radius);
        gl.uniform1f(p.at("uIntro"), intro);
        gl.uniform2fv(p.at("uViewport"), viewport);
        gl.uniform2fv(p.at("uCenter"), center);
        gl.uniform1f(p.at("uScale"), radius * dpr);
        // The size of a pixel in scene radii.
        gl.uniform1f(p.at("uPixel"), 1 / (radius * dpr));
        for (const [name, value] of Object.entries(own)) gl.uniform1f(p.at(name), value);
      };

      bloom?.begin();
      gl.clear(gl.COLOR_BUFFER_BIT);

      use(glow);
      gl.uniform1f(glow.at("uExtent"), scene.extent);
      gl.bindBuffer(gl.ARRAY_BUFFER, glowBuffer);
      const corner = gl.getAttribLocation(glow.program, "aCorner");
      gl.enableVertexAttribArray(corner);
      gl.vertexAttribPointer(corner, 2, gl.FLOAT, false, 0, 0);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      gl.disableVertexAttribArray(corner);

      use(stars);
      gl.uniform1f(stars.at("uPx"), (dpr * radius) / 380);
      gl.uniform1f(stars.at("uFocal"), Math.max(width, height) * dpr * 0.9);
      gl.uniform1f(stars.at("uMaxPoint"), maxPoint);
      gl.bindBuffer(gl.ARRAY_BUFFER, starBuffer);
      const enabled: number[] = [];
      for (const [name, size, offset] of scene.layout) {
        const at = gl.getAttribLocation(stars.program, name);
        if (at < 0) continue;
        gl.enableVertexAttribArray(at);
        gl.vertexAttribPointer(at, size, gl.FLOAT, false, scene.stride * 4, offset * 4);
        enabled.push(at);
      }
      gl.drawArrays(gl.POINTS, 0, starCount);
      for (const at of enabled) gl.disableVertexAttribArray(at);
      bloom?.end(scene.bloom ?? 0);
    };

    // The scene leans towards the mouse, on the sign-in screen and behind the pages alike.
    const controls = tilt({ rest: scene.rest, still: still || !!scene.fixed });

    // Animation time in seconds. It only advances while the page is shown, by at most a tenth of a
    // second per frame, so coming back after a while carries on where it left off. Behind the pages
    // it runs at half speed, to stay in the background; the tilt towards the mouse keeps real time.
    const pace = backdrop ? 0.5 : 1;
    const gap = backdrop ? 30 : (scene.gap ?? 0);
    let clock = still ? (scene.stillTime ?? 0) : 0;
    let prev: number | null = null;
    let last = 0;
    const loop = (ms: number) => {
      const dt = prev === null ? 0 : Math.min(ms - prev, 100) / 1000;
      clock += dt * pace;
      prev = ms;
      controls.step(dt);
      // The backdrop moves slowly; half the frame rate is plenty and saves power.
      if (ms - last >= gap) {
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
    draw(clock);
    if (!document.hidden) start();
    return () => {
      stop();
      observer.disconnect();
      document.removeEventListener("visibilitychange", onVisibility);
      controls.dispose();
      gl.deleteBuffer(starBuffer);
      gl.deleteBuffer(glowBuffer);
      gl.deleteProgram(stars.program);
      gl.deleteProgram(glow.program);
      bloom?.dispose();
    };
  }, [scene, backdrop]);

  return <canvas ref={ref} className={className} aria-hidden="true" />;
}
