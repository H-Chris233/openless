// Less Computer 全屏彩虹「跑马灯」描边浮层（独立窗口 window=less-computer-glow）。
//
// 思路（参考开源 MacEdgeLight / Apple Edge Light）：一个全屏、点击穿透、置顶、透明的
// 覆盖窗，沿屏幕圆角画一圈旋转的彩虹环 + 柔光，营造「整屏被 AI 点亮」的氛围感。
// 纯视觉：pointer-events:none，且后端对该窗口 set_ignore_cursor_events(true)。
// 仅 macOS 实际显示（后端只在 macOS show/hide 这个窗口）。

const glowCss = `
@property --lcg-angle { syntax: '<angle>'; initial-value: 0deg; inherits: false; }
@keyframes lcg-spin    { to { --lcg-angle: 360deg; } }
@keyframes lcg-breathe { 0%, 100% { opacity: .55; } 50% { opacity: 1; } }
@keyframes lcg-glow-breathe { 0%, 100% { opacity: .35; } 50% { opacity: .7; } }

html, body, #root { background: transparent !important; margin: 0; height: 100%; overflow: hidden; }

/* 旋转的彩虹环：conic-gradient 首尾同色（无断层）→ mask 抠出一圈描边 → 绕 --lcg-angle 旋转。 */
.lcg-ring {
  position: fixed;
  inset: 0;
  pointer-events: none;
  border-radius: var(--lcg-radius, 40px);
  padding: var(--lcg-width, 5px);
  background: conic-gradient(from var(--lcg-angle),
    #ff2d78, #ff8a00, #ffd400, #38e08a, #00c2ff, #7a5cff, #ff2d78);
  -webkit-mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0);
          mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0);
  -webkit-mask-composite: xor;
          mask-composite: exclude;
  animation: lcg-spin 6s linear infinite, lcg-breathe 5s ease-in-out infinite;
  filter: saturate(1.15) blur(0.4px);
}

/* 柔光层：贴着屏幕边往内渗的同色辉光（体积感），让描边不是一条硬线而是「发光」。 */
.lcg-soft {
  position: fixed;
  inset: 0;
  pointer-events: none;
  border-radius: var(--lcg-radius, 40px);
  box-shadow:
    inset 0 0 38px 4px rgba(124, 92, 255, 0.20),
    inset 0 0 90px 18px rgba(0, 194, 255, 0.10);
  animation: lcg-glow-breathe 5s ease-in-out infinite;
}

@media (prefers-reduced-motion: reduce) {
  .lcg-ring { animation: none; opacity: .8; }
  .lcg-soft { animation: none; }
}
`;

if (typeof document !== 'undefined' && !document.getElementById('less-computer-glow-style')) {
  const tag = document.createElement('style');
  tag.id = 'less-computer-glow-style';
  tag.textContent = glowCss;
  document.head.appendChild(tag);
}

export function LessComputerGlow() {
  return (
    <>
      <div className="lcg-soft" aria-hidden />
      <div className="lcg-ring" aria-hidden />
    </>
  );
}
