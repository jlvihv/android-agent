<script lang="ts">
  /**
   * 设置页「联网搜索」一栏：**没有输入框**。
   *
   * 搜索走 DeepSeek 官方的 Search API（`core/src/search.rs`），key 只认「模型提供商 →
   * DeepSeek」那一行，而且必须是 **platform.deepseek.com 的官方 key** ——
   * OpenCode Go / Zen 或中转站的 key 打那个端点是 401（实测过，见 docs/09）。
   * 所以平时聊天走 OpenCode Go 的话，这一栏仍是「未配置」，得单独填一个官方 key。
   */
  import { api } from "../lib/api";

  type Config = {
    /** 走的是哪家（核心那边是常量，不是用户可选的） */
    provider: string;
    configured: boolean;
    keyHint: string | null;
  };

  let cfg = $state<Config | null>(null);
  let err = $state("");

  async function load() {
    try {
      cfg = await api<Config>("/api/websearch/config");
    } catch (e) {
      err = String(e).replace(/^Error: /, "");
    }
  }

  // 和别的 Section 一个写法：`load` 取出来、`$effect` 只负责触发
  $effect(() => {
    void load();
  });
</script>

{#if cfg}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">联网搜索</h2>
  <div class="rounded-2xl bg-surface px-4 py-3.5">
    <p class="text-row">
      {cfg.provider}
      {#if cfg.configured}
        <span class="text-accent">已配置 {cfg.keyHint ?? ""}</span>
      {:else}
        <span class="text-muted">未配置</span>
      {/if}
    </p>
    <p class="pt-1.5 text-meta leading-relaxed text-faint">
      用的是「模型提供商 → DeepSeek」里的 key，必须是
      <a href="https://platform.deepseek.com/api_keys" class="text-accent"
        >platform.deepseek.com</a
      >
      的官方 key（OpenCode Go / Zen、中转站的 key 打不通搜索端点，会 401）。按成功的请求计费（5
      元 / 1000 次，搜不到不计费）。
      {#if !cfg.configured}
        去上面那一栏填一个官方 key 就能用。
      {/if}
    </p>
  </div>
  {#if err}<p class="px-1 pt-2 text-xs text-bad">{err}</p>{/if}
{/if}
