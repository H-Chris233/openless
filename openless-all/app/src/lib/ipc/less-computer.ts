import { invokeOrMock } from "./shared"

/** 用户点 ✕ / 按 Esc 关闭 Less Computer 浮窗（隐藏窗口）。 */
export function lessComputerWindowDismiss(): Promise<void> {
    return invokeOrMock("less_computer_window_dismiss", undefined, () => undefined)
}

/** 内联审批卡的 Approve / Deny 回执。token 关联到等待中的拦截动作。 */
export function lessComputerApprove(
    token: string,
    approved: boolean,
): Promise<void> {
    return invokeOrMock(
        "less_computer_approve",
        { token, approved },
        () => undefined,
    )
}

/** 前端按内容测高后回传，后端 clamp + bottom-anchored 重新摆放浮窗。 */
export function lessComputerWindowResize(height: number): Promise<void> {
    return invokeOrMock(
        "less_computer_window_resize",
        { height },
        () => undefined,
    )
}

/** 浮窗打字输入：文字指令直接进入 Less Computer 执行链（与语音同护栏/审批/连续会话）。 */
export function lessComputerSubmitText(text: string): Promise<void> {
    return invokeOrMock(
        "less_computer_submit_text",
        { text },
        () => undefined,
    )
}
