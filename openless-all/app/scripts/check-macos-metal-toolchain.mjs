import { spawnSync } from "node:child_process"

if (process.platform !== "darwin") process.exit(0)

const result = spawnSync("xcrun", ["--find", "metal"], {
    encoding: "utf8",
})

if (result.status === 0 && result.stdout?.trim()) process.exit(0)

console.error(`
OpenLess 的 Qwen3-ASR MLX 后端需要 Apple MetalToolchain。
当前 Xcode 未找到 Metal 编译器，请先执行：

  xcodebuild -downloadComponent MetalToolchain

完成后验证：

  xcrun --find metal

然后重新运行：

  pnpm tauri dev
`)
process.exit(1)
