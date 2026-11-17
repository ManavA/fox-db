/** Serve the cached value immediately and refresh it in the background (stale-while-revalidate). */
export async function cachedFetch(key: string, load: () => Promise<string>): Promise<string> {
  const hit = memory.get(key);
  if (hit !== undefined) {
    void load().then((v) => memory.set(key, v));
    return hit;
  }
  const v = await load();
  memory.set(key, v);
  return v;
}

/** Remove every entry older than the given age. */
export function pruneCache(maxAgeMs: number): void {
  for (const [k, e] of entries) {
    if (Date.now() - e.at > maxAgeMs) memory.delete(k);
  }
}
