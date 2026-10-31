import './global.css';

import type { Metadata, Viewport } from 'next';
import type { ReactNode } from 'react';
import { Provider } from './provider';

export const metadata: Metadata = {
  description:
    'Developer documentation for Hekma, a Ktesio project: the control plane for agent workloads. Today it supervises the lifecycle of AI agents, meters real token usage and enforces budgets.',
  metadataBase: new URL('https://hekma.ktesio.dev'),
  openGraph: {
    description:
      'Developer documentation for Hekma, a Ktesio project: the control plane for agent workloads. Today it supervises the lifecycle of AI agents, meters real token usage and enforces budgets.',
    images: ['/assets/hekma-banner.png'],
    siteName: 'Hekma Docs',
    title: 'Hekma Docs',
    type: 'website',
    url: '/',
  },
  title: {
    default: 'Hekma Docs',
    template: '%s | Hekma Docs',
  },
};

export const viewport: Viewport = {
  initialScale: 1,
  width: 'device-width',
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <body className="flex min-h-screen flex-col">
        <Provider>{children}</Provider>
      </body>
    </html>
  );
}
