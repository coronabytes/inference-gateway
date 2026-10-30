import { api } from "../api";
import type { ConfirmRequest } from "./ui";

/**
 * Prompt logging: offers to delete the stored prompts and responses of a key or project
 * (`path` is `/keys/{id}` or `/projects/{id}`) now, rather than at the end of their retention.
 * `always` opens the dialog even when nothing is stored, to say so; otherwise it opens only when
 * something is. `done` runs after a delete.
 */
export async function offerDeleteStoredPrompts(
  path: string,
  name: string,
  confirm: (r: ConfirmRequest) => void,
  { always = false, done }: { always?: boolean; done?: () => Promise<void> } = {},
) {
  const { stored } = await api<{ stored: number }>(`${path}/bodies`);
  if (!stored) {
    if (always) confirm({ title: "Stored prompts", message: <>No prompts or responses of <strong>{name}</strong> are stored.</>, confirmLabel: "OK", action: async () => {} });
    return;
  }
  confirm({
    title: "Delete stored prompts",
    message: (
      <>
        Delete the {stored === 1 ? "stored prompt and response" : `${stored.toLocaleString()} stored prompts and responses`} of <strong>{name}</strong> now? Kept,
        they are removed at the end of their retention. Their entries stay in the request log.
      </>
    ),
    confirmLabel: "Delete",
    action: async () => {
      await api(`${path}/bodies`, { method: "DELETE" });
      await done?.();
    },
  });
}
