/**
 * Model vendors' logos, from LobeHub's icons (@lobehub/icons-static-svg 1.101.0, MIT, see
 * assets/vendors/LICENSE), trimmed of their size and title. They are drawn inline, so the
 * single-colour ones take the text colour of either theme.
 */
import alibaba from "../assets/vendors/alibaba.svg" with { type: "text" };
import amazon from "../assets/vendors/amazon.svg" with { type: "text" };
import anthropic from "../assets/vendors/anthropic.svg" with { type: "text" };
import cohere from "../assets/vendors/cohere.svg" with { type: "text" };
import deepseek from "../assets/vendors/deepseek.svg" with { type: "text" };
import google from "../assets/vendors/google.svg" with { type: "text" };
import meta from "../assets/vendors/meta.svg" with { type: "text" };
import microsoft from "../assets/vendors/microsoft.svg" with { type: "text" };
import mistral from "../assets/vendors/mistral.svg" with { type: "text" };
import moonshot from "../assets/vendors/moonshot.svg" with { type: "text" };
import openai from "../assets/vendors/openai.svg" with { type: "text" };
import xai from "../assets/vendors/xai.svg" with { type: "text" };
import zhipu from "../assets/vendors/zhipu.svg" with { type: "text" };

/** The logo of each vendor the hub suggests. */
const LOGO: Record<string, string> = {
  Alibaba: alibaba,
  Amazon: amazon,
  Anthropic: anthropic,
  Cohere: cohere,
  DeepSeek: deepseek,
  Google: google,
  Meta: meta,
  Microsoft: microsoft,
  "Mistral AI": mistral,
  "Moonshot AI": moonshot,
  OpenAI: openai,
  xAI: xai,
  "Zhipu AI": zhipu,
};

/** A vendor's logo in a rounded square; without one, the first letter of `fallback`. */
export function VendorLogo({ vendor, fallback }: { vendor: string; fallback: string }) {
  const svg = LOGO[vendor];
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
  const svg = LOGO[vendor];
  if (!svg) return <span className="vendor-icon letter">{vendor.charAt(0).toUpperCase()}</span>;
  return <span className="vendor-icon" dangerouslySetInnerHTML={{ __html: svg }} />;
}
