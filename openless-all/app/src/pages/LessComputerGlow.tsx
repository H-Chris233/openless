// Less Computer 全屏彩虹「跑马灯」描边浮层（独立窗口 window=less-computer-glow）。
//
// 思路（参考开源 MacEdgeLight / Apple Edge Light，自写实现）：一个全屏、点击穿透、置顶、
// 透明的覆盖窗，沿屏幕边缘画一圈「贴边清晰、向内高斯模糊羽化」的流动彩虹辉光。
//   - 细带 conic-gradient 彩虹环，首尾同色；
//   - 重 blur() 高斯模糊 → 既把环糊成柔光、又彻底抹平 conic 起始角的断层；
//   - 容器 overflow:hidden 把外溢的模糊裁在屏幕最外缘 → 边缘清晰、内部模糊（苹果那种感觉）；
//   - 旋转 = 跑马灯流动，呼吸 = 明暗起伏。
// 纯视觉：pointer-events:none，后端再 set_ignore_cursor_events(true)。仅 macOS 显示。

const glowCss = `
@property --lcg-angle { syntax: '<angle>'; initial-value: 0deg; inherits: false; }
@keyframes lcg-spin    { to { --lcg-angle: 360deg; } }
@keyframes lcg-breathe { 0%, 100% { opacity: .58; } 50% { opacity: 1; } }

html, body, #root { background: transparent !important; margin: 0; height: 100%; overflow: hidden; }

/* 全屏裁剪容器：圆角贴合屏幕物理圆角；overflow:hidden 把外溢模糊裁在屏幕边缘。 */
.lcg-root {
  position: fixed;
  inset: 0;
  pointer-events: none;
  overflow: hidden;
  border-radius: var(--lcg-radius, 34px);
}
/* 彩虹环：略微跨出屏幕边缘(inset 负) → 满色部分压在最外缘(清晰)，blur 向内羽化(模糊)。
   细带(padding) + 重 blur = 苹果 Edge Light 的柔光，而不是一条又粗又硬的描边。 */
.lcg-root::before {
  content: '';
  position: absolute;
  inset: -5px;
  border-radius: calc(var(--lcg-radius, 34px) + 5px);
  padding: 9px;
  background: conic-gradient(from var(--lcg-angle),
    #ff3b6b, #ff8a3d, #ffe14d, #46e08a, #36c6ff, #9b6bff, #ff3b6b);
  -webkit-mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0);
          mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0);
  -webkit-mask-composite: xor;
          mask-composite: exclude;
  filter: blur(22px) saturate(1.3);
  animation: lcg-spin 6s linear infinite, lcg-breathe 4.5s ease-in-out infinite;
}

@media (prefers-reduced-motion: reduce) {
  .lcg-root::before { animation: none; opacity: .85; }
}
`;

if (typeof document !== 'undefined' && !document.getElementById('less-computer-glow-style')) {
  const tag = document.createElement('style');
  tag.id = 'less-computer-glow-style';
  tag.textContent = glowCss;
  document.head.appendChild(tag);
}

export function LessComputerGlow() {
  return <div className="lcg-root" aria-hidden />;
}
