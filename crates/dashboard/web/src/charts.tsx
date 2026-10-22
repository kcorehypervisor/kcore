export type Slice = { label: string; value: number; color: string };

export function Donut({ slices }: { slices: Slice[] }) {
  const usable = slices.filter((slice) => slice.value > 0);
  const total = usable.reduce((sum, slice) => sum + slice.value, 0);
  if (total <= 0) return <p className="muted">No data yet.</p>;
  const radius = 42;
  const circumference = 2 * Math.PI * radius;
  let offset = 0;
  const gap = usable.length > 1 ? 2 : 0;
  return (
    <div className="chart-layout">
      <svg viewBox="0 0 120 120" width="148" height="148" role="img" aria-label={usable.map((slice) => `${slice.label} ${slice.value}`).join(", ")}>
        <circle cx="60" cy="60" r={radius} fill="none" stroke="#1a1a1a" strokeWidth="14" />
        {usable.map((slice) => {
          const portion = (slice.value / total) * circumference;
          const length = Math.max(0, portion - gap);
          const element = (
            <circle
              key={slice.label}
              cx="60"
              cy="60"
              r={radius}
              fill="none"
              stroke={slice.color}
              strokeWidth="14"
              strokeDasharray={`${length} ${circumference - length}`}
              strokeDashoffset={-offset}
              transform="rotate(-90 60 60)"
            />
          );
          offset += portion;
          return element;
        })}
        <text x="60" y="64" textAnchor="middle" fill="#ffffff" fontSize="18" fontWeight="700">
          {total}
        </text>
      </svg>
      <div className="legend">
        {usable.map((slice) => (
          <div key={slice.label}>
            <span className="swatch" style={{ background: slice.color }} />
            <span>{slice.label}</span>
            <strong>{slice.value}</strong>
          </div>
        ))}
      </div>
    </div>
  );
}

export function Bars({ items, label }: { items: { label: string; value: number; color?: string }[]; label: string }) {
  if (items.length === 0) return <p className="muted">No data yet.</p>;
  const max = Math.max(...items.map((item) => item.value), 1);
  return (
    <div className="bars" role="img" aria-label={label}>
      {items.map((item) => (
        <div className="bar-row" key={item.label}>
          <span>{item.label}</span>
          <div className="bar-track">
            <div className="bar-fill" style={{ width: `${(item.value / max) * 100}%`, background: item.color ?? "#6366f1" }} />
          </div>
          <strong>{item.value}</strong>
        </div>
      ))}
    </div>
  );
}

export function Gauge({ percent, caption, id }: { percent: number; caption?: string; id: string }) {
  const bounded = Math.max(0, Math.min(100, percent));
  const radius = 46;
  const circumference = 2 * Math.PI * radius;
  const length = (bounded / 100) * circumference;
  return (
    <div className="gauge">
      <svg viewBox="0 0 120 120" width="128" height="128" role="img" aria-label={`${Math.round(bounded)} percent${caption ? `, ${caption}` : ""}`}>
        <defs>
          <linearGradient id={id} x1="0" y1="0" x2="1" y2="1">
            <stop offset="0" stopColor="#6366f1" />
            <stop offset="1" stopColor="#8b5cf6" />
          </linearGradient>
        </defs>
        <circle cx="60" cy="60" r={radius} fill="none" stroke="#27272a" strokeWidth="10" />
        <circle
          cx="60"
          cy="60"
          r={radius}
          fill="none"
          stroke={`url(#${id})`}
          strokeWidth="10"
          strokeLinecap="round"
          strokeDasharray={`${length} ${circumference - length}`}
          transform="rotate(-90 60 60)"
        />
        <text x="60" y="66" textAnchor="middle" fill="#ffffff" fontSize="20" fontWeight="700">
          {Math.round(bounded)}%
        </text>
      </svg>
      {caption ? <span className="muted">{caption}</span> : null}
    </div>
  );
}

export function Meter({ label, percent }: { label: string; percent: number }) {
  const bounded = Math.max(0, Math.min(100, percent));
  return (
    <div className="meter">
      <span>{label}</span>
      <div className="bar-track" role="meter" aria-valuenow={Math.round(bounded)} aria-valuemin={0} aria-valuemax={100} aria-label={label}>
        <div className="bar-fill" style={{ width: `${bounded}%` }} />
      </div>
      <strong>{Math.round(bounded)}%</strong>
    </div>
  );
}
