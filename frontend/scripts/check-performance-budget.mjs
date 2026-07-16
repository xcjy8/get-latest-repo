import { gzipSync } from 'node:zlib';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';

const assetsDirectory = new URL('../dist/assets/', import.meta.url);
const files = await readdir(assetsDirectory);
const javascriptFiles = files.filter((file) => file.endsWith('.js') && !file.includes('.worker-'));
const gzipBytes = (
  await Promise.all(
    javascriptFiles.map(async (file) => gzipSync(await readFile(join(assetsDirectory.pathname, file))).byteLength),
  )
).reduce((total, size) => total + size, 0);
const budgetBytes = 180 * 1024;

if (gzipBytes > budgetBytes) {
  throw new Error(`首屏 JavaScript 为 ${(gzipBytes / 1024).toFixed(1)} KiB，超过 180 KiB 性能预算`);
}

console.log(`✓ 首屏 JavaScript ${(gzipBytes / 1024).toFixed(1)} KiB gzip，满足 180 KiB 预算`);
