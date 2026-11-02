// Puts Scalar's API reference into public/docs/scalar.js, for the API docs at /docs.
//
// Scalar ships a prebuilt, self-contained browser bundle. Installing the npm package would add
// some 800 packages to this one; instead this fetches only the pinned release's tarball from the
// registry, takes that one file out, and checks it against the hash below. To update: change
// VERSION, run `bun scripts/fetch-scalar.ts`, and copy the hash it reports.
//
// The dev server and the build run it first; it does nothing when the file is already there.

import path from "node:path";

const VERSION = "1.76.2";
const SHA256 = "1a98470f928ab9315b4f727147245384fab807ba121b55f5b73414f12332932a";
const MEMBER = "package/dist/browser/standalone.js";
const TARBALL = `https://registry.npmjs.org/@scalar/api-reference/-/api-reference-${VERSION}.tgz`;

const target = path.join(import.meta.dir, "..", "public", "docs", "scalar.js");
const sha256 = (bytes: Uint8Array) => new Bun.CryptoHasher("sha256").update(bytes).digest("hex");

export async function fetchScalar() {
  const file = Bun.file(target);
  if ((await file.exists()) && sha256(await file.bytes()) === SHA256) return;

  const res = await fetch(TARBALL);
  if (!res.ok) throw new Error(`${TARBALL}: ${res.status} ${res.statusText}`);
  const bytes = extract(Bun.gunzipSync(await res.bytes()), MEMBER);
  const got = sha256(bytes);
  if (got !== SHA256) {
    throw new Error(`@scalar/api-reference@${VERSION} ${MEMBER} has SHA-256 ${got}, expected ${SHA256}`);
  }
  await Bun.write(target, bytes);
  console.log(`fetched Scalar ${VERSION} into ${path.relative(process.cwd(), target)}`);
}

/** One file from an uncompressed tar archive (ustar, as npm writes them). */
function extract(tar: Uint8Array, name: string) {
  const text = new TextDecoder();
  for (let at = 0; at + 512 <= tar.length;) {
    const header = tar.subarray(at, at + 512);
    const field = (from: number, len: number) => text.decode(header.subarray(from, from + len)).replace(/\0.*$/s, "");
    const base = field(0, 100);
    if (!base) break;
    const prefix = field(345, 155);
    const size = parseInt(field(124, 12).trim() || "0", 8);
    const full = prefix ? `${prefix}/${base}` : base;
    if (full === name) return tar.subarray(at + 512, at + 512 + size);
    at += 512 + Math.ceil(size / 512) * 512;
  }
  throw new Error(`${name} is not in the @scalar/api-reference@${VERSION} package`);
}

if (import.meta.main) await fetchScalar();
