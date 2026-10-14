/**
 * 模型清单（`GET /api/models` 的 `groups`）的**前端**缓存。
 *
 * 为什么必须放在组件外面：「选择模型」面板是 `{#if sheet}` 挂出来的，关一次
 * 就销毁一次（`App.svelte`）。清单如果存在组件里，每次打开都是
 * `groups = []` + `loading = true`，于是面板先按「一行：载入中…」的高度弹出来，
 * 十几毫秒后 38 行到了再长高 —— 用户看到的就是那一下布局跳动。
 *
 * 后端**有**缓存（`AppState::models_of`，10 分钟），它也确实在生效：
 * 命中缓存 15ms 返回，强制直连上游要 1.7–4s。但它解决不了前端这一跳 ——
 * 跳的根源是「组件重新挂载 → 第一帧先渲染 loading」，不是网络慢。
 * 要消灭它，**第一帧就得有数据**，也就是缓存得比组件活得久。
 *
 * TTL 和后端对齐（10 分钟）：过了不是不能用，而是先拿旧的渲染、背后悄悄重拉，
 * 界面不闪也不跳。设置页每关一次会带来一个新的 `rev`（Key 可能改了），
 * 和缓存上记的对不上就强制重拉 —— 这是唯一需要「真的再拉一次」的场合。
 */

export type Model = { id: string; name: string; context_window?: number | null };
/** 一家服务商的模型（`GET /api/models` 的 `groups`） */
export type Group = { id: string; name: string; models: Model[]; error?: string };

/** 和后端 `AppState::models_of` 的 TTL 一致 */
const TTL = 10 * 60 * 1000;

class ModelCache {
  groups = $state<Group[]>([]);
  /** 当前在用的是哪家（服务商 id） */
  provider = $state<string | null>(null);
  /**
   * 拉过一次了没。只有从没拉过才显示「载入中…」—— 有旧数据时重拉是静默的，
   * 不然每次打开都先闪一下载入中，又回到老问题。
   */
  loaded = $state(false);
  err = $state('');

  #at = 0;
  /** 这份清单是对着哪个配置版本拉的（`rev`，见 ModelSheet 的 props） */
  #rev: number | null = null;
  /** 并发调用合成一次请求：面板重开、设置页关掉可能挤在一起 */
  #inflight: Promise<void> | null = null;

  /**
   * 确保手上有清单可渲染。新鲜就直接返回（一个请求都不发），
   * 过期就先留着旧的（调用方第一帧照常渲染）、背后重拉。
   */
  async load(rev: number): Promise<void> {
    if (this.#fresh(rev)) return;
    if (this.#inflight) return this.#inflight;
    this.#inflight = this.#fetch(rev).finally(() => (this.#inflight = null));
    return this.#inflight;
  }

  #fresh(rev: number): boolean {
    // 上次失败过就不算新鲜：面板下次打开再试一次，别把一次网络抖动
    // 缓存成 10 分钟的「拉不到模型」
    return this.loaded && this.err === '' && this.#rev === rev && Date.now() - this.#at < TTL;
  }

  /** 面板里切了服务商：本地先认了，不为一个字段重拉整张清单
   *  （清单是「各家有哪些模型」，跟选谁无关）。 */
  setProvider(id: string | null): void {
    this.provider = id;
  }

  /**
   * 这个模型的上下文上限（输入框那圆环的分母）。`GET /api/models` 里
   * models.dev 给了就有，没收录的模型就是 `null` —— 那时圆环只显示用量，
   * 不编一个假上限出来。
   *
   * 先在当前那家找，再退到全场找：`provider` 可能是 null（老接口/未配置）。
   */
  contextWindow(modelId: string | undefined): number | null {
    if (!modelId) return null;
    const here = this.groups.find((g) => g.id === this.provider);
    const found =
      here?.models.find((m) => m.id === modelId) ??
      this.groups.flatMap((g) => g.models).find((m) => m.id === modelId);
    return found?.context_window ?? null;
  }

  async #fetch(rev: number): Promise<void> {
    try {
      const r = await fetch('/api/models');
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      const d = (await r.json()) as { groups?: Group[]; provider?: string | null };
      this.groups = d.groups ?? [];
      this.provider = d.provider ?? null;
      this.err = '';
      this.#at = Date.now();
      this.#rev = rev;
    } catch (e) {
      // 拉失败不清空手上的清单：旧的总比空的好，界面也就不用先跳一下
      this.err = String(e);
    } finally {
      this.loaded = true;
    }
  }
}

export const models = new ModelCache();
