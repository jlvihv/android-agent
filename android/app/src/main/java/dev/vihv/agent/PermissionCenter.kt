package dev.vihv.agent

import android.Manifest
import android.app.Activity
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/**
 * 设置页「手机权限」那一栏：每一项**现在开没开** + **去开**（[status] / [request]）。
 *
 * 为什么要集中一处：这些授权散在系统设置的五六个角落（无障碍、所有文件、通知使用权、
 * 电池优化、修改系统设置、运行时权限……），有的能弹框，有的只能把人送过去。以前只在
 * 首次启动时各问一次，没给就再也找不到入口了。
 *
 * 页面经 `AgentHost`（MainActivity 的 JS 桥）调这两个；从系统设置回来后页面自己再问一次状态。
 */
object PermissionCenter {
    private const val TAG = "PermissionCenter"

    /** 请求码：和 MainActivity（2..5）、AgentPermissions 的都错开 */
    const val REQUEST_CODE = 0x4A12

    private const val PREFS = "permission_center"

    /** 一项授权。`kind` 决定怎么去开 */
    private class Item(
        val key: String,
        val title: String,
        /** 开了能干什么（给人看的一句话） */
        val desc: String,
        val granted: (Context) -> Boolean?,
        val kind: Kind,
        /** 运行时权限（kind = RUNTIME） */
        val perms: Array<String> = emptyArray(),
        /** 跳哪个设置页（kind = SETTINGS） */
        val intent: ((Context) -> Intent)? = null,
    )

    private enum class Kind { RUNTIME, SETTINGS, INFO, SHIZUKU }

    private fun pkgUri(ctx: Context) = Uri.parse("package:${ctx.packageName}")

    private fun appDetails(ctx: Context) =
        Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, pkgUri(ctx))

    private fun notificationSettings(ctx: Context) =
        Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, ctx.packageName)

    private fun has(ctx: Context, vararg perms: String) =
        perms.all { ctx.checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED }

    private val items: List<Item> by lazy {
        buildList {
            add(Item(
                "accessibility", "无障碍服务", "点按、滑动、按键、截屏（agent tap / key / screenshot）",
                { accessibilityOn(it) }, Kind.SETTINGS,
                intent = { Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS) },
            ))
            add(Item(
                "files", "所有文件访问", "工作目录放在内部存储的 Agent 文件夹，能读写 /sdcard",
                { AgentApp.hasAllFilesAccess(it) }, Kind.SETTINGS,
                intent = {
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                        Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, pkgUri(it))
                    } else {
                        appDetails(it)
                    }
                },
            ))
            add(Item(
                "notifications", "通知", "回合跑完提醒、后台运行时的进度",
                { it.getSystemService(NotificationManager::class.java)?.areNotificationsEnabled() },
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) Kind.RUNTIME else Kind.SETTINGS,
                perms = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    arrayOf(Manifest.permission.POST_NOTIFICATIONS)
                } else {
                    emptyArray()
                },
                intent = { notificationSettings(it) },
            ))
            add(Item(
                "battery", "忽略电池优化", "切到后台、锁屏后长任务不被系统冻住",
                { it.getSystemService(PowerManager::class.java)?.isIgnoringBatteryOptimizations(it.packageName) },
                Kind.SETTINGS,
                intent = { Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, pkgUri(it)) },
            ))
            if (Build.VERSION.SDK_INT >= 36) {
                add(Item(
                    "live_updates", "实时更新", "回合进行中在状态栏 / 锁屏上显示进度（Android 16）",
                    { LiveUpdates.active(it) }, Kind.SETTINGS,
                    intent = {
                        Intent("android.settings.APP_NOTIFICATION_PROMOTION_SETTINGS")
                            .putExtra(Settings.EXTRA_APP_PACKAGE, it.packageName)
                    },
                ))
            }
            // Shizuku：**不是一个系统权限**，而是「App → 另一个 App」的授权，
            // 所以既不能走 `requestPermissions`，也没有设置页跳转：
            // 状态从 `ShizukuManager.snapshot()` 现问，点一下就是拉起授权界面
            // （没装 / 没运行就先把它打开，别让人对着一个永远的「未授权」）。
            add(Item(
                "shizuku", "Shizuku（特权命令）", "以 adb / root 身份跑系统命令（agent shizuku）",
                {
                    when (dev.vihv.agent.shizuku.ShizukuManager.snapshot().state) {
                        dev.vihv.agent.shizuku.ShizukuManager.State.READY -> true
                        else -> false
                    }
                },
                Kind.SHIZUKU,
            ))
            add(Item(
                "notification_listener", "通知读取", "读通知栏里的通知（agent notifications）",
                { AgentNotificationListener.enabled(it) }, Kind.SETTINGS,
                intent = { AgentNotificationListener.settingsIntent(it) },
            ))
            add(Item(
                "contacts", "通讯录", "查联系人（agent contacts）",
                { has(it, Manifest.permission.READ_CONTACTS) }, Kind.RUNTIME,
                perms = arrayOf(Manifest.permission.READ_CONTACTS),
            ))
            add(Item(
                "calendar", "日历", "查看和增改日程（agent calendar）",
                { has(it, Manifest.permission.READ_CALENDAR, Manifest.permission.WRITE_CALENDAR) }, Kind.RUNTIME,
                perms = arrayOf(Manifest.permission.READ_CALENDAR, Manifest.permission.WRITE_CALENDAR),
            ))
            add(Item(
                "sms", "短信", "读短信（agent sms）",
                { has(it, Manifest.permission.READ_SMS) }, Kind.RUNTIME,
                perms = arrayOf(Manifest.permission.READ_SMS),
            ))
            add(Item(
                "location", "定位", "当前位置（agent location）",
                { has(it, Manifest.permission.ACCESS_FINE_LOCATION) || has(it, Manifest.permission.ACCESS_COARSE_LOCATION) },
                Kind.RUNTIME,
                perms = arrayOf(Manifest.permission.ACCESS_FINE_LOCATION, Manifest.permission.ACCESS_COARSE_LOCATION),
            ))
            add(Item(
                "write_settings", "修改系统设置", "调屏幕亮度（agent brightness set）",
                { Settings.System.canWrite(it) }, Kind.SETTINGS,
                intent = { Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS, pkgUri(it)) },
            ))
            add(Item(
                "clipboard", "剪贴板",
                "Android 只让前台的 App 读剪贴板，没有能授予的权限：App 在前台时 agent clipboard get 读得到，切到后台读到的是空的",
                { null }, Kind.INFO,
            ))
        }
    }

    /** 每一项的现状（给页面）：`state` = granted | missing | info */
    fun status(ctx: Context): JSONArray {
        val out = JSONArray()
        for (it in items) {
            val g = runCatching { it.granted(ctx) }.getOrNull()
            out.put(
                JSONObject()
                    .put("key", it.key)
                    .put("title", it.title)
                    .put("desc", it.desc)
                    .put("state", if (it.kind == Kind.INFO) "info" else if (g == true) "granted" else "missing"),
            )
        }
        return out
    }

    /**
     * 去开 `key` 那一项：能弹框的弹框，不能的送去对应的设置页。
     *
     * 运行时权限被拒过、系统已经不再弹框（「不再询问」）的，直接送去 App 详情页 ——
     * 不然点了没反应，用户以为坏了。
     */
    fun request(activity: Activity, key: String) {
        val item = items.firstOrNull { it.key == key } ?: return
        val prefs = activity.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val intent = when (item.kind) {
            Kind.INFO -> return
            // 没就绪就别弹授权框（弹了也没用）：先把管理器 App 打开，让人自己去启动 / 装
            Kind.SHIZUKU -> {
                val state = dev.vihv.agent.shizuku.ShizukuManager.snapshot().state
                if (state == dev.vihv.agent.shizuku.ShizukuManager.State.NEED_PERMISSION) {
                    dev.vihv.agent.shizuku.ShizukuManager.requestPermission()
                    return
                }
                dev.vihv.agent.shizuku.ShizukuManager.openManager(activity)
                return
            }
            Kind.SETTINGS -> item.intent?.invoke(activity)
            Kind.RUNTIME -> {
                val asked = prefs.getBoolean(key, false)
                val blocked = asked && item.perms.none { activity.shouldShowRequestPermissionRationale(it) }
                if (item.perms.isNotEmpty() && !blocked) {
                    prefs.edit().putBoolean(key, true).apply()
                    runCatching { activity.requestPermissions(item.perms, REQUEST_CODE) }
                        .onFailure { Log.w(TAG, "弹不了权限框：$key", it) }
                    return
                }
                item.intent?.invoke(activity) ?: appDetails(activity)
            }
        } ?: return
        runCatching { activity.startActivity(intent) }
            .recoverCatching { activity.startActivity(appDetails(activity)) }
            .onFailure { Log.w(TAG, "打不开设置页：$key", it) }
    }

    private fun accessibilityOn(ctx: Context): Boolean =
        Settings.Secure.getString(ctx.contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES)
            .orEmpty()
            .contains(ctx.packageName)
}
