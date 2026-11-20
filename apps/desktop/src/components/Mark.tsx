/** The Kern mark: a geometric K on a 32-unit grid (see `brand/README.md`). */
export function Mark({ size = 18 }: { size?: number }) {
  return (
    <svg className="mark" width={size} height={size} viewBox="0 0 32 32" aria-hidden="true">
      <rect x="6" y="6" width="4.75" height="20" rx="1" fill="currentColor" />
      <path
        d="M10.75 16 L20.75 6 M10.75 16 L20.75 26"
        fill="none"
        stroke="currentColor"
        strokeWidth="4.75"
      />
    </svg>
  );
}
