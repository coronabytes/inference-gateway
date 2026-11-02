// Builds the console into dist/: index.html at the root, the scripts, styles and other files it
// loads under assets/ (named after their content hash, so the gateway lets browsers cache them for
// good), and public/ copied over as it is.
//
// Every text file also gets a brotli-compressed copy next to it (`index.html.br`), at the highest
// quality, which the gateway sends to browsers that accept it instead of compressing on each request.
// Files that brotli does not make smaller get none.

import { cp, readdir, rm } from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";
import { brotliCompress as brotliCallback, constants } from "node:zlib";
import { fetchScalar } from "./fetch-scalar";

const root = path.join(import.meta.dir, "..");
const outdir = path.join(root, "dist");
const brotliCompress = promisify(brotliCallback);
const COMPRESSIBLE = new Set([".html", ".js", ".css", ".svg", ".json", ".txt"]);

await fetchScalar();
await rm(outdir, { recursive: true, force: true });

const result = await Bun.build({
  entrypoints: [path.join(root, "index.html")],
  outdir,
  target: "browser",
  minify: true,
  splitting: true,
  sourcemap: "none",
  publicPath: "/",
  define: { "process.env.NODE_ENV": JSON.stringify("production") },
  naming: { entry: "[name].[ext]", chunk: "assets/[name]-[hash].[ext]", asset: "assets/[name]-[hash].[ext]" },
});
if (!result.success) {
  for (const log of result.logs) console.error(log);
  process.exit(1);
}
await cp(path.join(root, "public"), outdir, { recursive: true });

const files = (await readdir(outdir, { recursive: true, withFileTypes: true }))
  .filter((f) => f.isFile())
  .map((f) => path.join(f.parentPath, f.name))
  .sort();
const kB = (bytes: number) => `${(bytes / 1024).toFixed(1).padStart(8)} kB`;
await Promise.all(
  files.map(async (file) => {
    const bytes = await Bun.file(file).bytes();
    let line = `${path.relative(root, file).replaceAll("\\", "/").padEnd(40)} ${kB(bytes.length)}`;
    if (COMPRESSIBLE.has(path.extname(file))) {
      const br = await brotliCompress(bytes, {
        params: {
          [constants.BROTLI_PARAM_QUALITY]: constants.BROTLI_MAX_QUALITY,
          [constants.BROTLI_PARAM_MODE]: constants.BROTLI_MODE_TEXT,
          [constants.BROTLI_PARAM_SIZE_HINT]: bytes.length,
        },
      });
      if (br.length < bytes.length) {
        await Bun.write(`${file}.br`, br);
        line += ` ${kB(br.length)} br`;
      }
    }
    return line;
  }),
).then((lines) => console.log(lines.join("\n")));
