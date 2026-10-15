<script lang="ts">
  /**
   * 设置页的「技能（Skill）」一栏：装了哪些、放在哪。只读 ——
   * 装一个 skill 就是往那个目录里放一个带 SKILL.md 的文件夹（让 AI 用 git clone
   * 装也行）。索引会进系统提示词，AI 遇到对应的事会先读 SKILL.md（见 core/src/skills.rs）。
   */
  type Skill = { name: string; description: string; path: string };

  let skills = $state<Skill[]>([]);
  let dir = $state<string | null>(null);
  let err = $state('');

  $effect(() => {
    void (async () => {
      try {
        const r = await fetch('/api/skills');
        if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
        const d = (await r.json()) as { dir: string | null; skills: Skill[] };
        skills = d.skills;
        dir = d.dir;
      } catch (e) {
        err = String(e);
      }
    })();
  });
</script>

{#if dir}
  <h2 class="px-1 pt-6 pb-2 text-meta text-muted">技能（Skill）</h2>
  {#if skills.length}
    <div class="overflow-hidden rounded-2xl bg-surface">
      {#each skills as s (s.path)}
        <div class="border-b-2 border-ink px-3.5 py-3 last:border-b-0">
          <span class="block truncate text-[17px]">{s.name}</span>
          <span class="line-clamp-2 block text-meta text-muted">{s.description}</span>
        </div>
      {/each}
    </div>
  {/if}
  <p class="px-1 pt-2 text-meta leading-relaxed text-faint">
    {skills.length ? '' : '还没有。'}放在 <code class="break-all">{dir}/&lt;名字&gt;/SKILL.md</code>
    （开头写 name 和 description）就会被 AI 用上。
  </p>
  {#if err}<p class="px-1 pt-2 text-xs text-bad">{err}</p>{/if}
{/if}
