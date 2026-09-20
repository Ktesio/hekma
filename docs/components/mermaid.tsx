'use client';

import { useEffect, useRef, useState } from 'react';

/**
 * Renders a Mermaid diagram (story 14-4's docs diagrams: the ACP transport
 * flow, the metering tiers, the command journey, session resume). The
 * `mermaid` package is imported DYNAMICALLY so it never lands in the
 * initial bundle — pages without diagrams pay nothing for it. Rendering is
 * strictly client-side (mermaid needs the DOM); before hydration completes
 * the raw chart is shown in a <pre> so the source is never lost — the same
 * content a static renderer would print.
 */
export function Mermaid({ chart }: { chart: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [svg, setSvg] = useState('');
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const mermaid = (await import('mermaid')).default;
      mermaid.initialize({
        startOnLoad: false,
        // `strict` sanitizes all labels/edges — the diagrams are static
        // repo content, but strict is the honest default for a docs site.
        securityLevel: 'strict',
        theme: 'neutral',
      });
      // A unique id per render: mermaid's renderer keys its internal SVG
      // registry by id, and two diagrams sharing an id collide.
      const id = `mermaid-${Math.random().toString(36).slice(2)}`;
      const { svg: rendered } = await mermaid.render(id, chart);
      if (!cancelled) setSvg(rendered);
    })().catch((e) => {
      if (!cancelled) {
        // Surfaced, not silent (the AI-18 shape, in miniature): a diagram
        // that fails to render shows its own source so nothing disappears.
        console.error('mermaid render failed:', e);
        setFailed(true);
      }
    });
    return () => {
      cancelled = true;
    };
  }, [chart]);

  if (failed) {
    return <pre className="docs-mermaid-source">{chart}</pre>;
  }

  return (
    <div
      ref={ref}
      className="docs-mermaid my-4 overflow-x-auto rounded-lg border bg-fd-card p-4 text-fd-card-foreground [&_svg]:mx-auto"
      // The SVG is mermaid-generated from this repo's own static chart
      // (securityLevel: strict) — not user input.
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  );
}
