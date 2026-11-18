/**
 * 让 `bun test` 能 import `.svelte.ts`（模块级的 `$state` / `$derived` / `$effect`）。
 *
 * 为什么需要：`lib/chat.svelte.ts` 把「一条会话的流 + 消息」搬到了组件外面，
 * 那块逻辑（切走不断流、切回来接着写）是最该有测试盯的 —— 而 `bun test` 不认
 * runes，直接 import 会 `ReferenceError: $state is not defined`。
 *
 * 做法和 `vite-plugin-svelte` 一样：先剥掉 TS 类型，再交给 `compileModule`
 * （它只吃 JS —— 直接喂 TS 会在 `import type` 上报 `js_parse_error`）。
 */
import { plugin } from "bun";
import { readFile } from "node:fs/promises";
import { compileModule } from "svelte/compiler";

const strip = new Bun.Transpiler({ loader: "ts" });

plugin({
  name: "svelte-runes",
  setup(build) {
    build.onLoad({ filter: /\.svelte\.ts$/ }, async (args) => {
      const src = await readFile(args.path, "utf8");
      const js = strip.transformSync(src);
      const out = compileModule(js, {
        generate: "client",
        filename: args.path,
      });
      return { contents: out.js.code, loader: "js" };
    });
  },
});
