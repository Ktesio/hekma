import defaultMdxComponents from 'fumadocs-ui/mdx';
import type { MDXComponents } from 'mdx/types';
import { isValidElement, type ComponentPropsWithoutRef, type ReactNode } from 'react';
import { Mermaid } from './mermaid';

const routeByMarkdownFile = new Map([
  ['README.md', '/'],
  ['get-started.md', '/get-started'],
  ['installation.md', '/installation'],
  ['troubleshooting.md', '/troubleshooting'],
  ['commands.md', '/commands'],
  ['manifest.md', '/manifest'],
  ['adapter-contract.md', '/adapter-contract'],
  ['lockfile.md', '/lockfile'],
  ['architecture.md', '/architecture'],
  ['metering-agents-you-dont-control.md', '/design/metering-agents-you-dont-control'],
  ['testing.md', '/testing'],
  ['contributing.md', '/contributing'],
  ['release-process.md', '/release-process'],
  ['github-repository-audit-checklist.md', '/github-repository-audit-checklist'],
  ['RELEASE_NOTES.md', '/release-notes'],
]);

function docsHref(href?: string) {
  if (!href) {
    return href;
  }

  if (
    href.startsWith('#') ||
    href.startsWith('http://') ||
    href.startsWith('https://') ||
    href.startsWith('mailto:')
  ) {
    return href;
  }

  if (href === '../CONTRIBUTING.md') {
    return 'https://github.com/Ktesio/hekma/blob/main/CONTRIBUTING.md';
  }

  const [path, fragment] = href.split('#', 2);
  const fileName = path.split('/').pop();
  const route = fileName ? routeByMarkdownFile.get(fileName) : undefined;

  if (!route) {
    return href;
  }

  return fragment ? `${route}#${fragment}` : route;
}

function DocsLink(props: ComponentPropsWithoutRef<'a'>) {
  const Anchor = defaultMdxComponents.a ?? 'a';

  return <Anchor {...props} href={docsHref(props.href)} />;
}

// Fumadocs highlights fenced code through nested token spans, so the code
// block's TEXT is recovered by walking the element tree — a plain
// `children.toString()` would return "[object Object]" for any highlighted
// block (and every fenced block is highlighted).
function codeText(node: ReactNode): string {
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(codeText).join('');
  if (isValidElement<{ children?: ReactNode }>(node)) {
    return codeText(node.props.children);
  }
  return '';
}

function codeLanguage(node: ReactNode): string {
  if (isValidElement<{ className?: string; children?: ReactNode }>(node)) {
    const match = /language-([\w-]+)/.exec(node.props.className ?? '');
    if (match) return match[1];
    return codeLanguage(node.props.children);
  }
  if (Array.isArray(node)) {
    for (const child of node) {
      const found = codeLanguage(child);
      if (found) return found;
    }
  }
  return '';
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    pre: (props: ComponentPropsWithoutRef<'pre'>) => {
      const language = codeLanguage(props.children);
      if (language === 'mermaid') {
        return <Mermaid chart={codeText(props.children)} />;
      }
      const Pre = defaultMdxComponents.pre ?? 'pre';
      return <Pre {...props} />;
    },
    a: DocsLink,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
