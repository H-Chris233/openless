// Marker.tsx — 会话内联标记（shadcn Marker 的纯 React 版）：状态更新、系统备注、
// 带底边框的行、带标签的分隔线。与消息流并排使用（LC 面板工具行 / 费用行等）。
//
// 颜色刻意 inherit：深色（LC 面板）与主题化（QA 面板）容器各自给色，
// 组件只负责结构与布局。流式状态配 role="status"（辅助技术播报）+ 调用方的
// 扫光 class（shimmer）。分隔线的两侧线条是装饰元素，不加 role="separator"
//（带文字的分隔线取文字为内容，separator role 反而会吞掉可见标签）。

import type { CSSProperties, ReactNode } from 'react';

type MarkerVariant = 'default' | 'border' | 'separator';

interface MarkerProps {
  variant?: MarkerVariant;
  /** 流式/进行中状态传 "status"，让辅助技术播报更新。 */
  role?: string;
  className?: string;
  style?: CSSProperties;
  children: ReactNode;
}

export function Marker({ variant = 'default', role, className, style, children }: MarkerProps) {
  if (variant === 'separator') {
    return (
      <div role={role} className={className} style={{ ...separatorStyle, ...style }}>
        <span aria-hidden style={separatorLineStyle} />
        <span style={{ flexShrink: 0 }}>{children}</span>
        <span aria-hidden style={separatorLineStyle} />
      </div>
    );
  }
  return (
    <div
      role={role}
      className={className}
      style={{
        ...rowStyle,
        ...(variant === 'border' ? borderStyle : null),
        ...style,
      }}
    >
      {children}
    </div>
  );
}

/** 装饰图标槽：对辅助技术隐藏，含义由相邻的 MarkerContent 承担。 */
export function MarkerIcon({ children, style }: { children: ReactNode; style?: CSSProperties }) {
  return (
    <span aria-hidden style={{ ...iconStyle, ...style }}>
      {children}
    </span>
  );
}

export function MarkerContent({
  children,
  className,
  style,
}: {
  children: ReactNode;
  className?: string;
  style?: CSSProperties;
}) {
  return (
    <span className={className} style={style}>
      {children}
    </span>
  );
}

const rowStyle: CSSProperties = {
  display: 'flex',
  alignItems: 'center',
  gap: 6,
  minWidth: 0,
};

const borderStyle: CSSProperties = {
  paddingBottom: 8,
  borderBottom: '0.5px solid rgba(127, 127, 127, 0.22)',
};

const separatorStyle: CSSProperties = {
  display: 'flex',
  alignItems: 'center',
  gap: 10,
  minWidth: 0,
};

const separatorLineStyle: CSSProperties = {
  flex: 1,
  height: 0.5,
  background: 'currentColor',
  opacity: 0.18,
};

const iconStyle: CSSProperties = {
  display: 'inline-flex',
  alignItems: 'center',
  justifyContent: 'center',
  flexShrink: 0,
};
