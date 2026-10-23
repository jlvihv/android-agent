/**
 * 收藏的页面（侧边栏那个「收藏的页面」+ 查看器右上角那颗星）。
 *
 * 为什么放在服务端（`/api/favorites`，见 core 的 `db.rs`）：这个 App 什么都不往
 * localStorage 里存（换台机器、换个浏览器就两套数据）。库在各机器的核心里，
 * 连着电脑时收藏的就是那台电脑的沙箱页面 —— 路径也是那台机器上的，本来就该
 * 跟着机器走。
 *
 * 为什么是模块级状态而不是 props：查看器里的星和抽屉里的列表是两个方向的入口
 * （一个加、一个列 / 删），中间隔着 App —— 和 `viewer` / `machines` 同一个理由。
 */

export type Favorite = {
  /** 沙箱里的绝对路径（`/workspace/report/index.html`），不带 `/sandbox/` 那一段 */
  path: string;
  /** 显示名：收藏时从页面 `<title>` 里取的，取不到就是文件名 */
  title: string;
  /** 收藏时刻（毫秒）。列表按它倒序 */
  createdAt: number;
};

class Favorites {
  /** 服务端那份列表（最近收的在前） */
  list = $state<Favorite[]>([]);
  #loaded = false;
  #loading: Promise<void> | null = null;

  has(path: string): boolean {
    return this.list.some((f) => f.path === path);
  }

  /**
   * 拉一次列表。同一份在路上的请求不重复发；已经拉到了就不再问
   * （`force` 只在「存失败了、要把界面拉回真实状态」时用）。
   */
  async load(force = false): Promise<void> {
    if (this.#loaded && !force) return;
    if (this.#loading) return this.#loading;
    this.#loading = (async () => {
      try {
        const r = await fetch("/api/favorites");
        if (r.ok) {
          this.list =
            ((await r.json()) as { favorites?: Favorite[] }).favorites ?? [];
          this.#loaded = true;
        }
      } catch {
        /* 后端不在就先用现有那份 */
      } finally {
        this.#loading = null;
      }
    })();
    return this.#loading;
  }

  /** 点星：已经在就用取消，不在就收。 */
  async toggle(path: string, title: string): Promise<void> {
    if (this.has(path)) await this.remove(path);
    else await this.add(path, title);
  }

  async add(path: string, title: string): Promise<void> {
    // 先上屏（点星要立刻有反应），服务端那份回来再覆盖（排序 / 截断以它为准）
    this.list = [
      { path, title, createdAt: Date.now() },
      ...this.list.filter((f) => f.path !== path),
    ];
    await this.#send("/api/favorites", { path, title });
  }

  async remove(path: string): Promise<void> {
    this.list = this.list.filter((f) => f.path !== path);
    await this.#send("/api/favorites/delete", { path });
  }

  async #send(url: string, body: unknown): Promise<void> {
    try {
      const r = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!r.ok) throw new Error(await r.text());
      this.list =
        ((await r.json()) as { favorites?: Favorite[] }).favorites ?? this.list;
      this.#loaded = true;
    } catch {
      // 没存进去：把界面拉回服务端的真实状态，别留一个假的收藏
      void this.load(true);
    }
  }
}

export const favorites = new Favorites();
