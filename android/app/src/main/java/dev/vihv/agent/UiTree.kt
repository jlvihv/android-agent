package dev.vihv.agent

import android.accessibilityservice.AccessibilityService
import android.graphics.Rect
import android.view.accessibility.AccessibilityNodeInfo
import org.json.JSONArray
import org.json.JSONObject

/**
 * 无障碍树 → 扁平 JSON（决策 D10）。
 *
 * 为什么扁平：树形嵌套在 Kotlin 侧序列化和 Rust 侧反序列化都更贵，
 * 而且我们的 agent loop 只关心"有哪些可交互元素"，不关心层级。
 * 用 `p`（父节点索引）表达结构，需要时再重建。
 *
 * 字段名必须和 `core/src/state.rs` 的 `UiSnapshot` 逐字对应。
 */
object UiTree {

    /** 上限。无障碍树能到几千个节点，全发过去既慢又撑爆上下文。 */
    private const val MAX_NODES = 500
    private const val MAX_DEPTH = 60
    private const val MAX_TEXT = 200

    /** 一个节点的定位信息，指令执行时要用 */
    data class NodeRef(val bounds: Rect, val viewId: String?)

    data class Snapshot(val json: String, val refs: Map<Int, NodeRef>)

    /**
     * 读一次当前界面。
     *
     * 返回 null 表示拿不到根节点（服务刚连上、或屏幕正被系统弹窗占用）。
     */
    fun snapshot(service: AccessibilityService, activity: String): Snapshot? {
        val root = service.rootInActiveWindow ?: return null
        try {
            val nodes = JSONArray()
            val refs = HashMap<Int, NodeRef>()
            val counter = intArrayOf(0)
            walk(root, -1, 0, nodes, refs, counter)

            val m = service.resources.displayMetrics
            val json = JSONObject()
                .put("ts", System.currentTimeMillis())
                .put("package", root.packageName?.toString().orEmpty())
                .put("activity", activity)
                .put("screen", JSONObject().put("w", m.widthPixels).put("h", m.heightPixels))
                .put("nodes", nodes)
                .toString()

            return Snapshot(json, refs)
        } finally {
            recycle(root)
        }
    }

    private fun walk(
        node: AccessibilityNodeInfo,
        parentIndex: Int,
        depth: Int,
        out: JSONArray,
        refs: MutableMap<Int, NodeRef>,
        counter: IntArray,
    ) {
        if (depth > MAX_DEPTH || counter[0] >= MAX_NODES) return

        val index = counter[0]++
        val bounds = Rect().also { node.getBoundsInScreen(it) }
        val viewId = node.viewIdResourceName?.takeIf { it.isNotEmpty() }

        out.put(nodeJson(node, index, parentIndex, bounds))
        // 只给"能定位到的"节点存引用，省内存
        refs[index] = NodeRef(bounds, viewId)

        for (k in 0 until node.childCount) {
            val child = node.getChild(k) ?: continue
            try {
                walk(child, index, depth + 1, out, refs, counter)
            } finally {
                recycle(child)
            }
        }
    }

    private fun nodeJson(
        node: AccessibilityNodeInfo,
        i: Int,
        p: Int,
        bounds: Rect,
    ): JSONObject {
        val o = JSONObject()
            .put("i", i)
            .put("p", p)
            .put("c", shortClass(node.className?.toString().orEmpty()))
            .put(
                "b",
                JSONArray().put(bounds.left).put(bounds.top).put(bounds.right).put(bounds.bottom),
            )
            .put(
                "a",
                JSONObject()
                    .put("clickable", node.isClickable)
                    .put("long_clickable", node.isLongClickable)
                    .put("editable", node.isEditable)
                    .put("scrollable", node.isScrollable)
                    .put("checkable", node.isCheckable)
                    .put("focusable", node.isFocusable)
                    .put("enabled", node.isEnabled)
                    .put("selected", node.isSelected),
            )

        // text 优先，没有就用 contentDescription —— 图标按钮只有后者
        val text = (node.text ?: node.contentDescription)?.toString()?.trim().orEmpty()
        if (text.isNotEmpty()) o.put("t", text.take(MAX_TEXT))

        node.viewIdResourceName?.let { if (it.isNotEmpty()) o.put("id", it) }

        val state = buildList {
            @Suppress("DEPRECATION")
            if (node.isCheckable) add(if (node.isChecked) "checked=true" else "checked=false")
            if (node.isPassword) add("password")
            if (!node.isEnabled) add("disabled")
            if (node.isSelected) add("selected")
            if (node.isFocused) add("focused")
        }
        if (state.isNotEmpty()) o.put("s", state.joinToString(" "))

        return o
    }

    /** `android.widget.Button` → `Button`。省一半上下文。 */
    private fun shortClass(fqcn: String): String {
        if (fqcn.isEmpty()) return "?"
        val dot = fqcn.lastIndexOf('.')
        return if (dot >= 0) fqcn.substring(dot + 1) else fqcn
    }

    /**
     * API 33 起 `recycle()` 是 no-op，但 26–32 上不调会泄漏 native 对象。
     * 一个屏幕几百个节点，不回收很快就把 pool 撑爆。
     */
    @Suppress("DEPRECATION")
    private fun recycle(node: AccessibilityNodeInfo) {
        runCatching { node.recycle() }
    }
}
