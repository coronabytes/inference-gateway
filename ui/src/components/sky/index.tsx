import { useState } from "react";
import type { SegmentOption } from "../controls";
import { AuroraIcon, BlackHoleIcon, GalaxyIcon, NoSkyIcon, PulsarIcon, SupernovaIcon } from "../ui";
import { aurora } from "./aurora";
import { blackhole } from "./blackhole";
import { SkyCanvas, type Scene } from "./engine";
import { galaxy } from "./galaxy";
import { pulsar } from "./pulsar";
import { StarField } from "./stars";
import { supernova } from "./supernova";

/** The skies the sign-in screen and, in the dark theme, the console's pages can have behind them:
 * a scene drawn with WebGL, or `none`, which is a still field of stars and needs no WebGL. */
export type SkyName = "galaxy" | "pulsar" | "supernova" | "aurora" | "blackhole" | "none";

const SCENES: Record<Exclude<SkyName, "none">, Scene> = { galaxy, pulsar, supernova, aurora, blackhole };

export const SKIES: SegmentOption<SkyName>[] = [
  { value: "galaxy", label: "Galaxy", icon: <GalaxyIcon /> },
  { value: "pulsar", label: "Pulsar", icon: <PulsarIcon /> },
  { value: "supernova", label: "Supernova", icon: <SupernovaIcon /> },
  { value: "aurora", label: "Aurora", icon: <AuroraIcon /> },
  { value: "blackhole", label: "Black hole", icon: <BlackHoleIcon /> },
  { value: "none", label: "None: still stars", icon: <NoSkyIcon /> },
];

/** The scenes alone, for the sign-in screen, which always has one. */
export const SCENE_SKIES = SKIES.filter((s) => s.value !== "none");

const KEY = "gateway-sky";

/** The sky of the sign-in screen: a random scene each time the page loads. Picking another there
 * lasts for the visit and is not remembered. */
export function useLoginSky(): [SkyName, (sky: SkyName) => void] {
  return useState<SkyName>(() => {
    const scenes = Object.keys(SCENES) as SkyName[];
    return scenes[Math.floor(Math.random() * scenes.length)];
  });
}

/** The sky behind the console's pages: the one picked in the user menu, which is remembered in
 * this browser. Until one is picked, it is `fallback`: the sky of the sign-in screen. */
export function useAppSky(fallback: SkyName): [SkyName, (sky: SkyName) => void] {
  const [picked, setPicked] = useState<SkyName | null>(() => {
    try {
      const saved = localStorage.getItem(KEY);
      if (SKIES.some((s) => s.value === saved)) return saved as SkyName;
    } catch {
      // Storage can be blocked; the sign-in screen's sky then.
    }
    return null;
  });
  const pick = (next: SkyName) => {
    try {
      localStorage.setItem(KEY, next);
    } catch {
      // Not remembered, but still switched for this visit.
    }
    setPicked(next);
  };
  return [picked ?? fallback, pick];
}

/** A sky: on the sign-in screen, or as the `backdrop` behind the console's pages. */
export function Sky({ name = "galaxy", className, backdrop }: { name?: SkyName; className?: string; backdrop?: boolean }) {
  if (name === "none") return <StarField className={className} />;
  return <SkyCanvas scene={SCENES[name]} className={className} backdrop={backdrop} />;
}
