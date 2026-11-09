/**
 * The sky without a scene: a still field of stars, drawn once as an SVG. It needs no WebGL and
 * costs nothing to keep on screen, for those who want the console calm, or whose machine the
 * moving skies slow down.
 */

interface Star {
  x: number;
  y: number;
  r: number;
  color: string;
  opacity: number;
}

/** The picture is this large, in its own units; it is scaled to cover whatever it is put in. */
const WIDTH = 1600;
const HEIGHT = 1000;

/** The stars: the same ones every time, from a random number generator that always starts at the
 * same place (mulberry32). */
const STARS: Star[] = (() => {
  let state = 20240229;
  const random = () => {
    state = (state + 0x6d2b79f5) | 0;
    let t = Math.imul(state ^ (state >>> 15), 1 | state);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const colors = ["#d1e8ff", "#d1e8ff", "#d1e8ff", "#ffffff", "#ffffff", "#ffbd94"];
  return Array.from({ length: 320 }, () => {
    // Mostly faint and small; a few bright ones.
    const bright = random() ** 3;
    return {
      x: random() * WIDTH,
      y: random() * HEIGHT,
      r: 0.45 + 1.1 * bright,
      color: colors[Math.floor(random() * colors.length)],
      opacity: 0.2 + 0.7 * bright,
    };
  });
})();

export function StarField({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox={`0 0 ${WIDTH} ${HEIGHT}`} preserveAspectRatio="xMidYMid slice" aria-hidden="true">
      {STARS.map((s, i) => (
        <g key={i} fill={s.color}>
          {/* The brightest have a soft glow around them. */}
          {s.r > 1.1 && <circle cx={s.x} cy={s.y} r={s.r * 3.5} opacity={s.opacity * 0.12} />}
          <circle cx={s.x} cy={s.y} r={s.r} opacity={s.opacity} />
        </g>
      ))}
    </svg>
  );
}
