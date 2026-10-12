/**
 * 与 Rust 侧 `core/src/state.rs` 一一对应的类型。
 * 改动这里时记得同步改 Rust（字段名靠 serde 保持一致）。
 */

export type Flags = {
  clickable: boolean;
  long_clickable: boolean;
  editable: boolean;
  scrollable: boolean;
  checkable: boolean;
  focusable: boolean;
  enabled: boolean;
  selected: boolean;
};

/** 剪枝后的扁平节点。`p` 是父节点索引，-1 为根。 */
export type Command =
  | { type: 'tap_xy'; x: number; y: number }
  | { type: 'swipe'; x1: number; y1: number; x2: number; y2: number; ms?: number }
  | { type: 'key'; name: 'back' | 'home' | 'recents' | 'enter' | string }
  | { type: 'launch'; package: string };

export type Hello = {
  port: number;
  version: string;
  platform: string;
  mock: boolean;
};

export type DeviceInfo = {
  connected: boolean;
  name: string;
  mock: boolean;
};

export type Ack = {
  id: string;
  ok: boolean;
  detail: string;
};

/** Rust `Event` 枚举（serde tag = "type"） */
export type AgentEvent =
  | ({ type: 'hello' } & Hello)
  | ({ type: 'device' } & DeviceInfo)
  | { type: 'log'; level: string; msg: string }
  | { type: 'agent'; chunk: Record<string, unknown> }
  | ({ type: 'ack' } & Ack);

/** 前端 → Rust 的 WS 消息 */
export type ClientMsg =
  | { type: 'command'; cmd: Command }
  | { type: 'log'; level: string; msg: string };
