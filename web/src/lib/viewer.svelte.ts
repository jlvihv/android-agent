/**
 * 沙箱文件查看器的全局开关。
 *
 * Markdown 里点 `sandbox:` 链接/图片时打开，App 根部渲染 <Viewer>。
 * 放成模块级状态是因为链接可能出现在任意深度的组件里（消息、思考面板），
 * 一层层往上抛回调太绕。
 */
export const viewer = $state<{ url: string | null; title: string }>({ url: null, title: '' });

export function openViewer(url: string, title = '') {
  viewer.title = title || decodeURIComponent(url.split(/[?#]/)[0].split('/').filter(Boolean).at(-1) ?? '');
  viewer.url = url;
}

export function closeViewer() {
  viewer.url = null;
}
