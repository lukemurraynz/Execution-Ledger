import type { ReactNode } from "react";

type PillTone = "success" | "warning" | "danger" | "neutral";

interface PillProps {
  tone?: PillTone;
  children: ReactNode;
}

export function Pill({ tone = "neutral", children }: PillProps) {
  return <span className={`pill ${tone}`}>{children}</span>;
}
