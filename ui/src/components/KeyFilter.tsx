import { useEffect, useState } from "react";
import { api, type Project, type VirtualKey } from "../api";
import { Select } from "./controls";
import type { SelectOption } from "./SearchSelect";

/** The key filter's value for a project: all of its keys. */
const PROJECT = "project:";

/** The key filter's value for the requests of keys that were deleted since. */
const DELETED = "deleted";

/** The key filter's options: each project, its keys indented under it, keys without one, then deleted keys. */
function keyOptions(keys: VirtualKey[], projects: Project[]): SelectOption[] {
  const tree = [...projects]
    .sort((a, b) => a.name.localeCompare(b.name))
    .flatMap((p) => {
      const own = keys.filter((k) => k.project_id === p.id);
      return [
        { value: PROJECT + p.id, label: p.name, note: `project · ${own.length} key${own.length === 1 ? "" : "s"}` },
        ...own.map((k) => ({ value: k.id, label: k.name, indent: true })),
      ];
    });
  const loose = keys.filter((k) => !k.project_id || !projects.some((p) => p.id === k.project_id));
  return [
    { value: "", label: "All" },
    ...tree,
    ...loose.map((k) => ({ value: k.id, label: k.name, group: tree.length ? "No project" : undefined })),
    // Under a heading of its own when there are others, so it does not read as a key without a project.
    { value: DELETED, label: "Deleted keys", group: tree.length ? "Deleted" : undefined },
  ];
}

/** Puts a `KeyFilter` choice in a query, as `key_id`, `project_id` or `deleted_keys`. */
export function setKeyFilter(q: URLSearchParams, value: string) {
  if (value.startsWith(PROJECT)) q.set("project_id", value.slice(PROJECT.length));
  else if (value === DELETED) q.set("deleted_keys", "true");
  else if (value) q.set("key_id", value);
}

/**
 * A filter bar's virtual key: one key, a project (all of its keys) or the keys that were deleted
 * since; "" is all. Pass the value to `setKeyFilter`.
 */
export function KeyFilter({ value, onChange }: { value: string; onChange: (value: string) => void }) {
  const [keys, setKeys] = useState<VirtualKey[]>([]);
  const [projects, setProjects] = useState<Project[]>([]);

  useEffect(() => {
    api<VirtualKey[]>("/keys")
      .then((k) => setKeys([...k].sort((a, b) => a.name.localeCompare(b.name))))
      .catch(() => setKeys([]));
    api<Project[]>("/projects")
      .then(setProjects)
      .catch(() => setProjects([]));
  }, []);

  return <Select caption="Key" label="Virtual key" value={value} onChange={onChange} options={keyOptions(keys, projects)} />;
}
