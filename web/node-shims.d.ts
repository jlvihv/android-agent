/**
 * 我们用到的**那几个** Node 内置模块的最小声明。
 *
 * 为什么不装 `@types/node`：这个包很大，而我们只需要 `zlib` / `fs` / `path` 里
 * 的六个函数，而且只在构建脚本里用（`vite.config.ts` 与 `scripts/precompress.mjs`）。
 * 它还会把 `process`、`Buffer`、`__dirname` 这些全局都塞进来 —— 前端代码里一旦
 * 误用（比如 `process.env`）就检查不出来了。
 *
 * `vite.config.ts` 顶上那句 `declare const process: …` 是同一个思路的先例。
 * 真需要更多 Node API 时，再往这里加，别顺手装整套类型。
 */
declare module "node:zlib" {
  export const constants: {
    BROTLI_PARAM_QUALITY: number;
    BROTLI_PARAM_LGWIN: number;
    BROTLI_PARAM_MODE: number;
    BROTLI_MODE_TEXT: number;
  };
  export function brotliCompressSync(
    buf: Uint8Array,
    opts?: unknown,
  ): Uint8Array;
  export function gzipSync(buf: Uint8Array, opts?: unknown): Uint8Array;
}

declare module "node:fs/promises" {
  export function readdir(
    p: string,
    opts: { withFileTypes: true },
  ): Promise<Array<{ name: string; isDirectory(): boolean }>>;
  export function readdir(p: string): Promise<string[]>;
  export function readFile(p: string): Promise<Uint8Array>;
  export function writeFile(p: string, data: Uint8Array): Promise<void>;
}

declare module "node:path" {
  export function join(...parts: string[]): string;
  export function extname(p: string): string;
}
