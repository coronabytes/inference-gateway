/** Speech deployments' voices: providers' presets, and the entry that lets others through. */

import type { Voice } from "./api";

/** The entry that lets voices not listed through. */
export const ANY_VOICE = "*";

/** OpenAI's built-in voices (also Azure OpenAI's). */
const OPENAI: Voice[] = [
  "alloy",
  "ash",
  "ballad",
  "coral",
  "echo",
  "fable",
  "onyx",
  "nova",
  "sage",
  "shimmer",
  "verse",
  "marin",
  "cedar",
].map((id) => ({ id }));

/** Mistral's Voxtral 3 preset voices, from its documentation. */
const MISTRAL: Voice[] = (
  [
    ["voice_478", "Sheila (English, US)"],
    ["voice_447", "Margaret (English, US)"],
    ["voice_485", "Rob (English, US)"],
    ["voice_3150", "James (English, US)"],
    ["voice_1787", "Mary (English, UK)"],
    ["voice_1786", "Sky (English, UK)"],
    ["voice_1788", "David (English, UK)"],
    ["voice_590", "Harriet (English, UK)"],
    ["voice_591", "Fred (English, UK)"],
    ["voice_3149", "Hugo (English, UK)"],
    ["voice_3147", "Howard (English, UK)"],
    ["voice_592", "Marie (French)"],
    ["voice_3137", "Manon (French)"],
    ["voice_589", "Lea (French)"],
    ["voice_3141", "Camille (French)"],
    ["voice_483", "Elena (Spanish)"],
    ["voice_481", "Heidi (German)"],
    ["voice_482", "Lorenzo (Italian)"],
    ["voice_480", "Gabriela (Portuguese)"],
    ["voice_479", "Maryam (Arabic)"],
    ["voice_3143", "Jordi (Catalan)"],
    ["voice_3139", "Ananya (Hindi)"],
    ["voice_3153", "Himanshu (Hindi)"],
    ["voice_3146", "Olivia (Hungarian)"],
    ["voice_3133", "Haruto (Japanese)"],
    ["voice_3136", "Sung (Korean)"],
    ["voice_3135", "Jin (Korean)"],
    ["voice_3145", "Lotte (Dutch)"],
    ["voice_3134", "Lars (Norwegian)"],
    ["voice_3154", "Leon (Polish)"],
    ["voice_3155", "Irina (Romanian)"],
    ["voice_3132", "Tatiana (Russian)"],
    ["voice_3138", "Oran (Turkish)"],
    ["voice_3140", "Taras (Ukrainian)"],
    ["voice_3144", "Huong (Vietnamese)"],
    ["voice_3142", "Mei (Chinese)"],
  ] as const
).map(([id, name]) => ({ id, name }));

/** A provider's preset voices, to fill the list with; none for providers without known ones. */
export function presetVoices(provider: string): Voice[] {
  if (provider === "mistral") return MISTRAL;
  if (provider === "openai" || provider === "azure") return OPENAI;
  return [];
}
