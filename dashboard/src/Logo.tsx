export function Logo({ id }: { id: string }) {
  return (
    <svg viewBox="0 0 32 32" width="22" height="22" aria-hidden="true">
      <defs>
        <linearGradient id={id} x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#6366f1" />
          <stop offset="1" stopColor="#8b5cf6" />
        </linearGradient>
      </defs>
      <rect width="32" height="32" rx="8" fill={`url(#${id})`} />
      <path d="M10 8v16M10 16l11-8M10 16l11 8" stroke="white" strokeWidth="2.2" fill="none" strokeLinecap="round" />
    </svg>
  );
}
