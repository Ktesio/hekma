import defaultMdxComponents from 'fumadocs-ui/mdx';
import type { MDXComponents } from 'mdx/types';
import type { ComponentPropsWithoutRef } from 'react';

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
    return 'https://github.com/Ktesio/ktesio/blob/main/CONTRIBUTING.md';
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

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    a: DocsLink,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
