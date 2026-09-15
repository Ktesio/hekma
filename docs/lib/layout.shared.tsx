import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';

export function baseOptions(): BaseLayoutProps {
  return {
    githubUrl: 'https://github.com/Ktesio/ktesio',
    links: [
      {
        active: 'url',
        text: 'Install',
        url: '/installation',
      },
      {
        active: 'url',
        text: 'Commands',
        url: '/commands',
      },
      {
        active: 'none',
        text: 'Crates.io',
        url: 'https://crates.io/crates/ktesio',
      },
    ],
    nav: {
      title: 'Ktesio',
    },
  };
}
