import type { StyleSystemPrompts, UserPreferences } from "../types"
export type { UpdateChannel } from "../types"
import { invokeOrMock } from "./shared"
import { mockSettings, mockDefaultStyleSystemPrompts, mockSetSettings } from "./mock-data"

export const BACKEND_CONTRACT_VERSION = "2.0.0"

export interface StartupSnapshot {
    contractVersion: string
    backend: { running: boolean }
}

export async function getStartupSnapshot(): Promise<StartupSnapshot> {
    const snapshot = await invokeOrMock<StartupSnapshot>("get_startup_snapshot", undefined, () => ({
        contractVersion: BACKEND_CONTRACT_VERSION,
        backend: { running: true },
    }))
    if (snapshot.contractVersion !== BACKEND_CONTRACT_VERSION) {
        throw new Error(`unsupported backend contract version: ${snapshot.contractVersion}`)
    }
    return snapshot
}

export function getSettings(): Promise<UserPreferences> {
    return invokeOrMock("get_settings", undefined, () => ({ ...mockSettings }))
}

export function getDefaultStyleSystemPrompts(): Promise<StyleSystemPrompts> {
    return invokeOrMock("get_default_style_system_prompts", undefined, () => ({
        ...mockDefaultStyleSystemPrompts,
    }))
}

export function setSettings(prefs: UserPreferences): Promise<void> {
    return invokeOrMock("set_settings", { prefs }, () => {
        mockSetSettings(prefs)
        return undefined
    })
}
