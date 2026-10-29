import type { Deployment, ReasoningEffort } from "./api";
import type { SelectOption } from "./components/SearchSelect";

/** Every reasoning effort, in order. */
export const EFFORTS: ReasoningEffort[] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/** The efforts every chat deployment of a model takes, from the price catalog; all of them while none is known. */
export function supportedEfforts(deployments: Deployment[], model: string): ReasoningEffort[] {
  const known = deployments.flatMap((d) => (d.mode === "chat" && d.model_name === model && d.reasoning_efforts ? [d.reasoning_efforts] : []));
  return known.length ? EFFORTS.filter((e) => known.every((k) => k.includes(e))) : EFFORTS;
}

/** A select's options: `defaultLabel` (value "") first, then every effort, those the model does not take disabled. */
export function effortOptions(supported: ReasoningEffort[], defaultLabel: string): SelectOption[] {
  return [
    { value: "", label: defaultLabel },
    ...EFFORTS.map((e) => ({
      value: e,
      label: `${e} effort`,
      disabled: !supported.includes(e),
      note: supported.includes(e) ? undefined : "not supported",
    })),
  ];
}
