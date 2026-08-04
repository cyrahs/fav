/** The bookmark mark from public/icon.svg, drawn in currentColor for use in text runs. */
export function Logo({ size = 20 }: { size?: number }) {
  return (
    <svg
      viewBox="0 0 64 64"
      width={size}
      height={size}
      fill="currentColor"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M20 11H44A4 4 0 0 1 48 15V55L32 43L16 55V15A4 4 0 0 1 20 11Z" />
    </svg>
  );
}
