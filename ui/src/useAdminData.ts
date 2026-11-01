import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "./api";

/**
 * What a page shows from the admin API: `path` read with `api`, or several paths read at once (`T`
 * is then the tuple of their answers, and one failure fails them all). It loads when the page opens
 * and again when `path` or `deps` change, so a filter belongs in the path's query. `reload` loads
 * again now, after a change was saved, and settles once the answer is in; it never throws.
 *
 * A failed load keeps the data it had and sets `error`, which a page also uses for its own failures
 * (`setError`), as its ErrorBanner shows; a load that succeeds clears it. An answer to a request that
 * a newer one replaced is dropped. A null `path` loads nothing and clears the data.
 */
export function useAdminData<T>(path: string | readonly string[] | null, deps: readonly unknown[] = []) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(path !== null);
  // `reload` keeps its identity and reads the path of the latest render.
  const latest = useRef(path);
  latest.current = path;
  const request = useRef(0);

  const reload = useCallback(async () => {
    const p = latest.current;
    const n = ++request.current;
    if (p === null) {
      setData(null);
      setLoading(false);
      return;
    }
    setLoading(true);
    try {
      const answer = typeof p === "string" ? await api<T>(p) : ((await Promise.all(p.map((x) => api<unknown>(x)))) as T);
      if (n !== request.current) return;
      setData(answer);
      setError(null);
    } catch (e) {
      if (n === request.current) setError((e as Error).message);
    } finally {
      if (n === request.current) setLoading(false);
    }
  }, []);

  const key = path === null ? null : typeof path === "string" ? path : path.join("\n");
  useEffect(() => {
    reload();
  }, [key, ...deps]);

  return { data, setData, error, setError, reload, loading };
}
