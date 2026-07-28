import type { ReactNode } from "react";

interface SectionHeadProps {
  eyebrow: string;
  title: string;
  note?: ReactNode;
}

export function SectionHead({ eyebrow, title, note }: SectionHeadProps) {
  return (
    <div className="section-head">
      <div>
        <div className="eyebrow">{eyebrow}</div>
        <h2>{title}</h2>
      </div>
      {note ? <p className="section-note">{note}</p> : null}
    </div>
  );
}
