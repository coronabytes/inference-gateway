import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { timeLabel } from "../format";

export interface Series {
  name: string;
  /** CSS color, normally a `var(--series-n)` token. */
  color: string;
  values: (number | null)[];
}

interface Props {
  kind: "line" | "bars";
  title: string;
  subtitle?: string;
  timestamps: number[];
  stepSecs: number;
  series: Series[];
  format: (n: number) => string;
  height?: number;
}

const M = { top: 10, right: 14, bottom: 24, left: 48 };

function niceTicks(max: number): number[] {
  if (!(max > 0)) return [0, 1];
  const rough = max / 3;
  const pow = 10 ** Math.floor(Math.log10(rough));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * pow).find((s) => s >= rough) ?? rough;
  const ticks = [];
  for (let v = 0; v <= max + step * 0.001; v += step) ticks.push(v);
  if (ticks[ticks.length - 1] < max) ticks.push(ticks[ticks.length - 1] + step);
  return ticks;
}

/** Bar with a 4px rounded data-end and a square baseline. */
function barPath(x: number, y: number, w: number, h: number, round: boolean): string {
  if (h <= 0) return "";
  const r = round ? Math.min(4, w / 2, h) : 0;
  return `M${x},${y + h}V${y + r}Q${x},${y} ${x + r},${y}H${x + w - r}Q${x + w},${y} ${x + w},${y + r}V${y + h}Z`;
}

export function TimeChart({ kind, title, subtitle, timestamps, stepSecs, series, format, height = 200 }: Props) {
  const wrap = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(600);
  const [hover, setHover] = useState<number | null>(null);

  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => setWidth(Math.max(240, Math.floor(e.contentRect.width))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const n = timestamps.length;
  const plotW = width - M.left - M.right;
  const plotH = height - M.top - M.bottom;
  const band = n > 0 ? plotW / n : plotW;

  const { ticks, yMax } = useMemo(() => {
    let max = 0;
    for (let i = 0; i < n; i++) {
      if (kind === "bars") {
        max = Math.max(max, series.reduce((s, ser) => s + (ser.values[i] ?? 0), 0));
      } else {
        for (const ser of series) max = Math.max(max, ser.values[i] ?? 0);
      }
    }
    const t = niceTicks(max);
    return { ticks: t, yMax: t[t.length - 1] || 1 };
  }, [series, n, kind]);

  const y = (v: number) => M.top + plotH - (v / yMax) * plotH;
  const xCenter = (i: number) => M.left + band * i + band / 2;

  const xLabels = useMemo(() => {
    const count = Math.max(2, Math.min(6, Math.floor(plotW / 110)));
    const idx = new Set<number>();
    for (let k = 0; k < count; k++) idx.add(Math.round((k * (n - 1)) / (count - 1)));
    return [...idx].filter((i) => i >= 0 && i < n);
  }, [plotW, n]);

  const onMove = (e: PointerEvent<SVGRectElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    const i = Math.floor(((e.clientX - rect.left) / rect.width) * n);
    setHover(Math.min(n - 1, Math.max(0, i)));
  };

  const onKey = (e: KeyboardEvent<SVGSVGElement>) => {
    if (e.key === "ArrowLeft") setHover((h) => Math.max(0, (h ?? n) - 1));
    else if (e.key === "ArrowRight") setHover((h) => Math.min(n - 1, (h ?? -1) + 1));
    else if (e.key === "Escape") setHover(null);
    else return;
    e.preventDefault();
  };

  const barW = Math.max(1, Math.min(24, band - 2));
  const tooltipLeft = hover != null ? Math.min(Math.max(xCenter(hover), 90), width - 90) : 0;

  return (
    <figure className="chart">
      <figcaption>
        <div className="chart-title">{title}</div>
        {subtitle && <div className="chart-subtitle">{subtitle}</div>}
        {series.length > 1 && (
          <ul className="legend">
            {series.map((s) => (
              <li key={s.name}>
                <svg width="14" height="10" aria-hidden="true">
                  {kind === "bars" ? (
                    <rect x="1" y="1" width="12" height="8" rx="2" fill={s.color} />
                  ) : (
                    <line x1="1" x2="13" y1="5" y2="5" stroke={s.color} strokeWidth="2" strokeLinecap="round" />
                  )}
                </svg>
                {s.name}
              </li>
            ))}
          </ul>
        )}
      </figcaption>
      <div className="chart-body" ref={wrap}>
        <svg
          width={width}
          height={height}
          role="img"
          aria-label={`${title}. Use left and right arrow keys to inspect values.`}
          tabIndex={0}
          onKeyDown={onKey}
          onBlur={() => setHover(null)}
        >
          {ticks.map((t) => (
            <g key={t}>
              <line className="grid" x1={M.left} x2={width - M.right} y1={y(t)} y2={y(t)} />
              <text className="tick" x={M.left - 8} y={y(t)} dy="0.32em" textAnchor="end">
                {format(t)}
              </text>
            </g>
          ))}
          <line className="baseline" x1={M.left} x2={width - M.right} y1={y(0)} y2={y(0)} />
          {xLabels.map((i) => (
            <text key={i} className="tick" x={xCenter(i)} y={height - 6} textAnchor="middle">
              {timeLabel(timestamps[i], stepSecs)}
            </text>
          ))}

          {kind === "bars" && hover != null && (
            <rect className="hover-band" x={M.left + band * hover} y={M.top} width={band} height={plotH} />
          )}

          {kind === "bars" &&
            timestamps.map((_, i) => {
              let acc = 0;
              const x = xCenter(i) - barW / 2;
              const top = series.reduce((last, s, si) => ((s.values[i] ?? 0) > 0 ? si : last), -1);
              return (
                <g key={i}>
                  {series.map((s, si) => {
                    const v = s.values[i] ?? 0;
                    if (v <= 0) return null;
                    const y0 = y(acc);
                    acc += v;
                    const y1 = y(acc);
                    // 2px surface gap between stacked segments
                    const gap = si > 0 && acc - v > 0 ? 2 : 0;
                    return <path key={s.name} d={barPath(x, y1, barW, Math.max(0, y0 - y1 - gap), si === top)} fill={s.color} />;
                  })}
                </g>
              );
            })}

          {kind === "line" &&
            series.map((s) => {
              let d = "";
              let pen = false;
              let last: [number, number] | null = null;
              s.values.forEach((v, i) => {
                if (v == null) {
                  pen = false;
                  return;
                }
                const px = xCenter(i);
                const py = y(v);
                d += `${pen ? "L" : "M"}${px},${py}`;
                pen = true;
                last = [px, py];
              });
              const end = last as [number, number] | null;
              const firstIdx = s.values.findIndex((v) => v != null);
              const area =
                series.length === 1 && end && firstIdx >= 0
                  ? `${d}L${end[0]},${y(0)}L${xCenter(firstIdx)},${y(0)}Z`
                  : null;
              return (
                <g key={s.name}>
                  {area && <path d={area} fill={s.color} opacity={0.1} />}
                  <path d={d} fill="none" stroke={s.color} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" />
                  {end && <circle className="ring" cx={end[0]} cy={end[1]} r={4} fill={s.color} />}
                </g>
              );
            })}

          {kind === "line" && hover != null && (
            <g>
              <line className="crosshair" x1={xCenter(hover)} x2={xCenter(hover)} y1={M.top} y2={M.top + plotH} />
              {series.map((s) =>
                s.values[hover] != null ? (
                  <circle key={s.name} className="ring" cx={xCenter(hover)} cy={y(s.values[hover] as number)} r={4} fill={s.color} />
                ) : null,
              )}
            </g>
          )}

          <rect
            x={M.left}
            y={M.top}
            width={plotW}
            height={plotH}
            fill="transparent"
            onPointerMove={onMove}
            onPointerLeave={() => setHover(null)}
          />
        </svg>
        {hover != null && (
          <div className="tooltip" style={{ left: tooltipLeft }} role="status">
            <div className="tooltip-time">{timeLabel(timestamps[hover], stepSecs)}</div>
            {series.map((s) => (
              <div className="tooltip-row" key={s.name}>
                <svg width="12" height="8" aria-hidden="true">
                  <line x1="1" x2="11" y1="4" y2="4" stroke={s.color} strokeWidth="2" strokeLinecap="round" />
                </svg>
                <strong>{s.values[hover] == null ? "–" : format(s.values[hover] as number)}</strong>
                <span>{s.name}</span>
              </div>
            ))}
          </div>
        )}
      </div>
    </figure>
  );
}
