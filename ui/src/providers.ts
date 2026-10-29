import type { Provider } from "./api";

export const PROVIDERS: Provider[] = ["openai", "azure", "bedrock", "anthropic", "vertex", "gemini", "mistral", "typesafe"];

export const PROVIDER_LABEL: Record<Provider, string> = {
  openai: "OpenAI-compatible",
  azure: "Azure AI Foundry",
  anthropic: "Anthropic",
  bedrock: "Amazon Bedrock",
  typesafe: "TypeSafe",
  mistral: "Mistral AI",
  vertex: "Google Vertex AI",
  gemini: "Google AI Studio",
};

/** Mistral's regional endpoints. */
export const MISTRAL_REGIONS: { value: string; label: string }[] = [
  { value: "global", label: "Global (api.mistral.ai)" },
  { value: "eu", label: "EU (api.eu.mistral.ai)" },
  { value: "us", label: "US (api.us.mistral.ai)" },
];

/** Vertex AI locations offered by name; any single region can be entered as well. */
export const VERTEX_REGIONS: { value: string; label: string }[] = [
  { value: "global", label: "Global" },
  { value: "eu", label: "EU multi-region" },
  { value: "us", label: "US multi-region" },
];
