/**
 * The slight lean of the galaxy (on the sign-in screen and behind the console's pages) towards the
 * mouse.
 *
 * Wherever the mouse is on the page, the scene turns a little to face it, by up to LEAN at a
 * screen edge, and back to straight when it leaves the window. It follows slowly, like something
 * heavy, so the camera never jumps. The lean sits on top of the scene's resting view rather than
 * adding to it, so it never builds up. With reduced motion there is no lean. Touch does nothing.
 */

/** 3x3 rotation matrices, column-major as WebGL takes them. */
export type Mat3 = number[];

export function multiply(a: Mat3, b: Mat3): Mat3 {
  const out: Mat3 = [];
  for (let col = 0; col < 3; col++)
    for (let row = 0; row < 3; row++)
      out.push(a[row] * b[col * 3] + a[3 + row] * b[col * 3 + 1] + a[6 + row] * b[col * 3 + 2]);
  return out;
}

export function rotateX(angle: number): Mat3 {
  const c = Math.cos(angle);
  const s = Math.sin(angle);
  return [1, 0, 0, 0, c, s, 0, -s, c];
}

export function rotateY(angle: number): Mat3 {
  const c = Math.cos(angle);
  const s = Math.sin(angle);
  return [c, 0, -s, 0, 1, 0, s, 0, c];
}

/** The lean towards the mouse, at most (radians), with the mouse at a screen edge. */
const LEAN = 0.12;

export interface Tilt {
  /** The resting view with the lean towards the mouse on top. */
  view(): Mat3;
  /** Move the lean on by `dt` seconds towards where the mouse is. */
  step(dt: number): void;
  dispose(): void;
}

export function tilt({ rest, still }: { rest: Mat3; still: boolean }): Tilt {
  let lean = { x: 0, y: 0 };
  let leanTo = { x: 0, y: 0 };
  const onHover = (e: PointerEvent) => {
    if (e.pointerType !== "mouse") return;
    leanTo = { x: (e.clientX / window.innerWidth - 0.5) * 2 * LEAN, y: (e.clientY / window.innerHeight - 0.5) * 2 * LEAN };
  };
  const onLeave = () => {
    leanTo = { x: 0, y: 0 };
  };
  if (!still) {
    window.addEventListener("pointermove", onHover);
    document.documentElement.addEventListener("mouseleave", onLeave);
  }
  return {
    view: () => (lean.x || lean.y ? multiply(rotateY(lean.x), multiply(rotateX(lean.y), rest)) : rest),
    step: (dt) => {
      const follow = 1 - Math.exp(-dt * 2.5);
      lean = { x: lean.x + (leanTo.x - lean.x) * follow, y: lean.y + (leanTo.y - lean.y) * follow };
    },
    dispose: () => {
      window.removeEventListener("pointermove", onHover);
      document.documentElement.removeEventListener("mouseleave", onLeave);
    },
  };
}
