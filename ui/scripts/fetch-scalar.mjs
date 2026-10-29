// Puts Scalar's API reference into public/docs/scalar.js, for the API docs at /docs.
//
// Scalar ships a prebuilt, self-contained browser bundle. Installing the npm package would add
// some 800 packages to this one; instead this fetches only the pinned release's tarball (npm
// checks it against the registry's integrity hash), takes that one file out, and checks it
// against the hash below. To update: change VERSION, run this, and copy the hash it reports.
//
// Runs before `npm run dev` and `npm run build`; does nothing when the file is already there.

import { execSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import zlib from "node:zlib";

const VERSION = "1.76.2";
const SHA256 = "1a98470f928ab9315b4f727147245384fab807ba121b55f5b73414f12332932a";
const MEMBER = "package/dist/browser/standalone.js";

const target = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "public", "docs", "scalar.js");
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

if (fs.existsSync(target) && sha256(fs.readFileSync(target)) === SHA256) process.exit(0);

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "scalar-"));
try {
  // Through a shell, where npm is a script on Windows too. Everything in the command is fixed
  // here, and the temporary path is quoted.
  const out = execSync(`npm pack @scalar/api-reference@${VERSION} --json --silent --pack-destination ${JSON.stringify(tmp)}`, {
    encoding: "utf8",
  });
  const [{ filename }] = JSON.parse(out);
  const bytes = extract(zlib.gunzipSync(fs.readFileSync(path.join(tmp, filename))), MEMBER);
  const got = sha256(bytes);
  if (got !== SHA256) {
    throw new Error(`@scalar/api-reference@${VERSION} ${MEMBER} has SHA-256 ${got}, expected ${SHA256}`);
  }
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, bytes);
  console.log(`fetched Scalar ${VERSION} into ${path.relative(process.cwd(), target)}`);
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}

/** One file from an uncompressed tar archive (ustar, as npm writes them). */
function extract(tar, name) {
  for (let at = 0; at + 512 <= tar.length; ) {
    const header = tar.subarray(at, at + 512);
    const field = (from, len) => header.subarray(from, from + len).toString("utf8").replace(/\0.*$/s, "");
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
