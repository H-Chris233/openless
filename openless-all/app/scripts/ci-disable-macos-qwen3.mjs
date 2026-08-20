import { readFileSync, writeFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import { fileURLToPath } from "node:url"

const appRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const cargoPath = resolve(appRoot, "src-tauri/Cargo.toml")
const cargo = readFileSync(cargoPath, "utf8")
const dependency = /^qwen3-asr-rs\s*=\s*\{[^\n]+\}\r?\n/m

if (!dependency.test(cargo)) {
    throw new Error(`未找到 macOS-only qwen3-asr-rs 依赖：${cargoPath}`)
}

writeFileSync(cargoPath, cargo.replace(dependency, ""))
console.log("[ci] disabled macOS-only qwen3-asr-rs dependency for this target")
