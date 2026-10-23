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
  | { type: "tap_xy"; x: number; y: number }
  | {
      type: "swipe";
      x1: number;
      y1: number;
      x2: number;
      y2: number;
      ms?: number;
    }
  | { type: "key"; name: "back" | "home" | "recents" | "enter" | string }
  | { type: "launch"; package: string };

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

/**
 * 一个回合此刻在干什么（Rust 的 `status::Snapshot`）。
 *
 * 页面只用 `sid` / `phase` / `active` 当「有事情在发生」的信号，状态文案和通知栏
 * 那一行是壳的事（见 docs/06）。字段还是照抄，免得以后想用的时候又得对一遍。
 */
export type TurnStatus = {
  sid: string;
  title: string;
  phase: "thinking" | "streaming" | "tool" | "done";
  tool?: string;
  detail?: string;
  started_at_ms: number;
  active: boolean;
  finished_at_ms?: number;
  ok: boolean;
  stopped: boolean;
};

/** Rust `Event` 枚举（serde tag = "type"） */
export type AgentEvent =
  | ({ type: "hello" } & Hello)
  | ({ type: "device" } & DeviceInfo)
  | { type: "log"; level: string; msg: string }
  | ({ type: "ack" } & Ack)
  /** 一个回合的状态播报（通知栏用）。**任何会话**的回合都会推到这里 */
  | { type: "turn"; status: TurnStatus }
  /** 「还有没有回合在跑」（全局的，不分会话）。起止各一条 */
  | { type: "turn_active"; active: boolean };

/** 前端 → Rust 的 WS 消息 */
export type ClientMsg =
  | { type: "command"; cmd: Command }
  | { type: "log"; level: string; msg: string };
