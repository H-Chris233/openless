import { isTauri, invokeOrMock } from "./shared"

export { isTauri }

export async function openExternal(url: string): Promise<void> {
    if (!isTauri) {
        window.open(url, "_blank", "noopener,noreferrer")
        return
    }
    try {
        const { open } = await import("@tauri-apps/plugin-shell")
        await open(url)
        return
    } catch (error) {
        console.warn("[external-open] shell plugin failed", error)
    }
    try {
        const { invoke } = await import("@tauri-apps/api/core")
        await invoke("open_external_url", { url })
        return
    } catch (error) {
        console.warn("[external-open] native fallback failed", error)
    }
    window.open(url, "_blank", "noopener,noreferrer")
}

/**
 * 让用户选 save 路径并把当前会话日志（openless.log）复制过去。
 * 浏览器开发模式下走 mock 不实际写盘。返回最终 save 的绝对路径，取消选择则返回 null。
 */
export async function exportErrorLog(
    suggestedFileName: string,
): Promise<string | null> {
    if (!isTauri) {
        return `~/Downloads/${suggestedFileName}`
    }
    const { save } = await import("@tauri-apps/plugin-dialog")
    const target = await save({
        defaultPath: suggestedFileName,
        filters: [{ name: "Log", extensions: ["log", "txt"] }],
    })
    if (!target) return null
    await invokeOrMock<void>(
        "export_error_log",
        { targetPath: target },
        () => undefined,
    )
    return target
}
