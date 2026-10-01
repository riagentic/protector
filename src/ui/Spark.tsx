import type { JSX } from "aio";

/** A tiny inline sparkline. `values` is oldest→newest. */
export function Spark(props: {
  values: number[];
  height?: number;
  max?: number;
  class?: string;
}): JSX.Element {
  const h = props.height ?? 28;
  const w = 100;
  const vals = props.values.length ? props.values : [0];
  const max = props.max ?? Math.max(1, ...vals);
  const n = vals.length;
  const pts = vals
    .map((v, i) => {
      const x = n <= 1 ? 0 : (i / (n - 1)) * w;
      const y = h - Math.max(0, Math.min(1, v / max)) * h;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg
      class={`spark ${props.class ?? ""}`}
      viewBox={`0 0 ${w} ${h}`}
      preserveAspectRatio="none"
      width="100%"
      height={h}
    >
      <polyline
        points={pts}
        fill="none"
        stroke="currentColor"
        stroke-width="1.6"
        vector-effect="non-scaling-stroke"
      />
    </svg>
  );
}
