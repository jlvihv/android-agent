<script lang="ts">
  /**
   * 设置页「联网搜索」一栏：Exa 的 key。
   *
   * 不填也能搜：走 Exa 免 key 的公开端点（按出口 IP 限额：2 次/秒、50 次/天，同一个
   * 出口的人共用）。用完了才需要填（见 docs/09）。key 本身服务端不回，只回末尾几位；
   * 环境变量 `EXA_API_KEY` 给的（桌面开发）这里改不了，只显示。
   */
  type Config = {
    configured: boolean;
    keyHint: string | null;
    fromEnv: boolean;
  };

  let cfg = $state<Config | null>(null);
  let key = $state("");
  let busy = $state(false);
  let err = $state("");

  async function load() {
    try {
      const r = await fetch("/api/websearch/config");
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      cfg = (await r.json()) as Config;
    } catch (e) {
      err = String(e).replace(/^Error: /, "");
    }
  }

  $effect(() => {
    void load();
  });

  async function save(value: string) {
    busy = true;
    err = "";
    try {
      const r = await fetch("/api/websearch/config", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ exa_api_key: value }),
      });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      key = "";
      await load();
    } catch (e) {
      err = String(e).replace(/^Error: /, "");
    } finally {
      busy = false;
    }
  }

  const input =
    "min-w-0 flex-1 rounded-2xl bg-surface px-4 py-3.5 text-[17px] outline-none placeholder:text-faint";
</script>

{#if cfg}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">联网搜索</h2>
  {#if cfg.fromEnv}
    <p class="rounded-2xl bg-surface px-3.5 py-3 text-[15px] text-muted">
      Exa key 由环境变量 <code>EXA_API_KEY</code> 提供{cfg.keyHint
        ? `（${cfg.keyHint}）`
        : ""}，这里改不了。
    </p>
  {:else}
    <div class="flex gap-2">
      <input
        bind:value={key}
        type="password"
        placeholder={cfg.configured
          ? `已保存 ${cfg.keyHint ?? ""}，填新的会替换`
          : "Exa API Key（可选）"}
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
        onkeydown={(e) => e.key === "Enter" && key.trim() && save(key.trim())}
        class={input}
      />
      <button
        onclick={() => save(key.trim())}
        disabled={busy || !key.trim()}
        class="shrink-0 rounded-full bg-fg px-5 text-[15px] font-medium text-ink disabled:opacity-40"
      >
        保存
      </button>
    </div>
    {#if cfg.configured}
      <button
        onclick={() => save("")}
        disabled={busy}
        class="px-1 pt-2 text-meta text-bad disabled:opacity-40"
      >
        清掉这个 key（回到免费额度）
      </button>
    {/if}
  {/if}
  <p class="px-1 pt-2 text-meta leading-relaxed text-faint">
    不填也能搜：免费额度按出口 IP 算（每天 50 次、每秒 2
    次，同一个网络的人共用），用完了再填。
    <a href="https://dashboard.exa.ai/api-keys" class="text-accent"
      >去 Exa 获取 key</a
    >
  </p>
  {#if err}<p class="px-1 pt-2 text-xs text-bad">{err}</p>{/if}
{/if}
