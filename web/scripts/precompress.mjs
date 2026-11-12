/**
 * 构建产物预压缩：给每个 `.js` / `.css` / `.html` / `.svg` 生成一份 `.br` 和一份
 * `.gz`，**和源文件一起**嵌进 .so（见 core/src/assets.rs）。
 *
 * 为什么在构建期而不是运行时：
 *
 *   - 运行时压（tower-http 的 CompressionLayer）每来一个新客户端都要重压一遍
 *     那份 199 KB 的 JS。本机回环上 CPU 不心疼，但手机是**每次冷启动**都要付，
 *     而且它挤在同一条线程上跟首屏渲染抢；
 *   - 构建期压一次，之后是 `Cow::Borrowed` 的静态切片 —— 服务端一个字节都不用算。
 *   - 压缩比：br 质量 11（构建期慢一点没关系）比档位 5 小 3% 左右。实测这份
 *     199 KB 的 JS：br ≈ 62 KB、gzip ≈ 67 KB（Lighthouse 之前估的 157 KiB 可省）。
 *
 * 为什么不用 `vite-plugin-compression`：它只吐 `.gz`/`.br` 文件，而**怎么选**取决
 * 于请求头，那部分仍要自己写；插件本身也就三十行，不如自己写清楚。
 *
 * 只压文本类：图片/字体本来就压过（再压反而变大），而且 `mime_guess` 那边也不该
 * 给它们发 `content-encoding`。
 */
import { brotliCompressSync, gzipSync, constants } from "node:zlib";
import { readdir, readFile, writeFile } from "node:fs/promises";
import { join, extname } from "node:path";

/** 值得压的扩展名（和 core/src/assets.rs 的 `COMPRESSIBLE` 必须一致） */
const TYPES = new Set([".js", ".mjs", ".css", ".html", ".svg", ".json", ".txt", ".map"]);

/** 太小的没必要压：多一个文件的元数据比省下的字节还值钱 */
const MIN_BYTES = 1024;

const BR = {
  params: {
    // 11 = 最大压缩。构建期只跑一次，慢几秒换更小的包
    [constants.BROTLI_PARAM_QUALITY]: 11,
    // 窗口 22（4 MB）够覆盖我们最大的单文件，再大就用默认的动态窗口
    [constants.BROTLI_PARAM_LGWIN]: 22,
    // 文本类：给通用文本模式，比默认稍好
    [constants.BROTLI_PARAM_MODE]: constants.BROTLI_MODE_TEXT,
  },
};

/** @param {string} dir @param {string[]} [out] @returns {Promise<string[]>} */
async function walk(dir, out = []) {
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) await walk(p, out);
    else if (TYPES.has(extname(e.name).toLowerCase())) out.push(p);
  }
  return out;
}

/** Vite 插件：`closeBundle` 时压一遍（写在 dist 落盘之后） */
export function precompress() {
  return {
    name: "agent-precompress",
    apply: "build",
    async closeBundle() {
      const dist = "dist";
      const files = await walk(dist);
      let raw = 0;
      let br = 0;
      let gz = 0;
      /** @type {Array<[string, number, number, number]>} 名字 / 原始 / br / gz */
      const rows = [];
      for (const f of files) {
        const buf = await readFile(f);
        if (buf.length < MIN_BYTES) continue;
        const b = brotliCompressSync(buf, BR);
        const g = gzipSync(buf, { level: 9 });
        await writeFile(`${f}.br`, b);
        await writeFile(`${f}.gz`, g);
        raw += buf.length;
        br += b.length;
        gz += g.length;
        rows.push([f.replace(`${dist}/`, ""), buf.length, b.length, g.length]);
      }
      if (!rows.length) return;
      /** @param {number} n */
      /** @param {number} n */
      const pct = (n) => `${((n / raw) * 100).toFixed(1)}%`;
      console.log(
        `\n预压缩 ${rows.length} 个文件：原始 ${(raw / 1024).toFixed(1)} KB → ` +
          `br ${(br / 1024).toFixed(1)} KB (${pct(br)}) / gz ${(gz / 1024).toFixed(1)} KB (${pct(gz)})`,
      );
      for (const [name, r, b] of rows) {
        console.log(
          `  ${name.padEnd(34)} ${(r / 1024).toFixed(1).padStart(7)} KB → br ${(b / 1024).toFixed(1).padStart(6)} KB`,
        );
      }
    },
  };
}
