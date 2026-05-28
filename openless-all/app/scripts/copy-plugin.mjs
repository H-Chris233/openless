import { copyFileSync, mkdirSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';
import { platform } from 'os';

// Only relevant on Linux — skip silently on other platforms
if (platform() !== 'linux') {
  process.exit(0);
}

const __dirname = dirname(fileURLToPath(import.meta.url));
const srcDir = join(__dirname, '..', 'scripts', 'linux-fcitx5-plugin', 'build_release');
const destDir = join(__dirname, 'src-tauri', 'linux-fcitx5-plugin');

const files = ['libopenless.so', 'openless.conf'];

for (const f of files) {
  const src = join(srcDir, f);
  const dest = join(destDir, f);
  try {
    mkdirSync(destDir, { recursive: true });
    copyFileSync(src, dest);
    console.log(`[copy:linux-plugin] copied ${f}`);
  } catch {
    console.log(`[copy:linux-plugin] ${f} not found — skipping`);
  }
}
