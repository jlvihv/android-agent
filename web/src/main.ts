import { mount } from 'svelte';
import './app.css';
import App from './App.svelte';
import { store } from './lib/store.svelte.ts';

// 立刻连 WS，别等组件挂载 —— 手机 WebView 从后台回来时能早点重连
store.connect();

const app = mount(App, { target: document.getElementById('app')! });

export default app;
