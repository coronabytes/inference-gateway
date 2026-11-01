/**
 * Model vendors' logos, from LobeHub's icons (@lobehub/icons-static-svg 1.101.0, MIT, see
 * assets/vendors/LICENSE), trimmed of their size and title. They are drawn inline, so the
 * single-colour ones take the text colour of either theme.
 */
const FILES = import.meta.glob<string>("../assets/vendors/*.svg", { query: "?raw", import: "default", eager: true });

/** The logo file of each vendor the hub suggests. */
const LOGO: Record<string, string> = {
  Alibaba: "alibaba",
  Amazon: "amazon",
  Anthropic: "anthropic",
  Cohere: "cohere",
  DeepSeek: "deepseek",
  Google: "google",
  Meta: "meta",
  Microsoft: "microsoft",
  "Mistral AI": "mistral",
  "Moonshot AI": "moonshot",
  OpenAI: "openai",
  xAI: "xai",
  "Zhipu AI": "zhipu",
};

/** A vendor's logo in a rounded square; without one, the first letter of `fallback`. */
export function VendorLogo({ vendor, fallback }: { vendor: string; fallback: string }) {
  const svg = FILES[`../assets/vendors/${LOGO[vendor]}.svg`];
  if (!svg) {
    return (
      <span className="vendor-mark" aria-hidden="true">
        {(vendor || fallback).charAt(0).toUpperCase()}
      </span>
    );
  }
  // Our own files, checked in: no scripts, only shapes.
  return <span className="vendor-mark logo" aria-hidden="true" dangerouslySetInnerHTML={{ __html: svg }} />;
}

/** A vendor's logo at text size, for a list option; without one, its first letter, and nothing without a vendor. */
export function VendorIcon({ vendor }: { vendor: string }) {
  const svg = FILES[`../assets/vendors/${LOGO[vendor]}.svg`];
  if (!svg) return <span className="vendor-icon letter">{vendor.charAt(0).toUpperCase()}</span>;
  return <span className="vendor-icon" dangerouslySetInnerHTML={{ __html: svg }} />;
}
