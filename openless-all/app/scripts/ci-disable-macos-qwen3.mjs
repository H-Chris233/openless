import { readFileSync, writeFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import { fileURLToPath } from "node:url"

const appRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const cargoPath = resolve(appRoot, "src-tauri/Cargo.toml")
const lockPath = resolve(appRoot, "src-tauri/Cargo.lock")
const cargo = readFileSync(cargoPath, "utf8")
const dependency = /^qwen3-asr-rs\s*=\s*\{[^\n]+\}\r?\n/m

if (!dependency.test(cargo)) {
    throw new Error(`未找到 macOS-only qwen3-asr-rs 依赖：${cargoPath}`)
}

writeFileSync(cargoPath, cargo.replace(dependency, ""))
const lock = readFileSync(lockPath, "utf8")
const openlessStart = lock.indexOf('name = "openless"')
const nextPackage = lock.indexOf("\n[[package]]", openlessStart + 1)
if (openlessStart < 0 || nextPackage < 0) {
    throw new Error(`未找到 openless Cargo.lock package block：${lockPath}`)
}
const openlessBlock = lock.slice(openlessStart, nextPackage)
const lockDependency = /^([ \t]*)"qwen3-asr-rs",\r?\n/m
if (!lockDependency.test(openlessBlock)) {
    throw new Error(`openless Cargo.lock package 未包含 qwen3-asr-rs：${lockPath}`)
}
const updatedBlock = openlessBlock.replace(lockDependency, "")
writeFileSync(lockPath, lock.slice(0, openlessStart) + updatedBlock + lock.slice(nextPackage))
console.log("[ci] disabled macOS-only qwen3-asr-rs dependency for this target")
