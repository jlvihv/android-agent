package dev.vihv.agent.shizuku

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * `decideState` 的三条分支 —— **纯函数，不碰 binder**。
 *
 * 为什么值得单独测：这三种状态在真机上要「装 Shizuku → 杀服务 → 撤销授权」
 * 才能一个一个复现，而每一种的用户体验完全不同（装 / 启动 / 授权）。
 * 判错一格的表现是「明明能用却说没装」，或者反过来「让人去装一个已经装了的」。
 */
class ShizukuManagerTest {

    private fun state(binder: Boolean, permitted: Boolean, installed: Boolean) =
        ShizukuManager.decideState(binder, permitted, installed)

    @Test
    fun `binder up and permitted is ready`() {
        assertEquals(
            ShizukuManager.State.READY,
            state(binder = true, permitted = true, installed = true),
        )
        // Sui 没有 APK：装了哪个包**不能**参与判断，否则活着的 Sui 会被判成"没装"
        assertEquals(
            ShizukuManager.State.READY,
            state(binder = true, permitted = true, installed = false),
        )
    }

    @Test
    fun `binder up but not permitted asks for permission`() {
        assertEquals(
            ShizukuManager.State.NEED_PERMISSION,
            state(binder = true, permitted = false, installed = true),
        )
        assertEquals(
            ShizukuManager.State.NEED_PERMISSION,
            state(binder = true, permitted = false, installed = false),
        )
    }

    @Test
    fun `no binder with a manager installed means start it`() {
        assertEquals(
            ShizukuManager.State.NOT_RUNNING,
            state(binder = false, permitted = false, installed = true),
        )
        // permitted 只看 binder 活着的时候才有意义：没 binder 时不可能是"没授权"
        assertEquals(
            ShizukuManager.State.NOT_RUNNING,
            state(binder = false, permitted = true, installed = true),
        )
    }

    @Test
    fun `nothing at all means install`() {
        assertEquals(
            ShizukuManager.State.NOT_INSTALLED,
            state(binder = false, permitted = false, installed = false),
        )
    }

    /**
     * binder 优先：这条是 Sui 能用起来的全部原因。
     *
     * 反过来写（先查包名）会把 Sui 判成 NOT_INSTALLED、并且在 `pingBinder()`
     * 之前就 short-circuit —— 活着的 binder 一次都没被问过。
     */
    @Test
    fun `a live binder wins over a missing manager package`() {
        assertEquals(
            ShizukuManager.State.READY,
            state(binder = true, permitted = true, installed = false),
        )
    }
}
