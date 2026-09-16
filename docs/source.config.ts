import { defineConfig, defineDocs } from 'fumadocs-mdx/config';

const files = [
  'README.md',
  'get-started.md',
  'installation.md',
  'migration.md',
  'troubleshooting.md',
  'commands.md',
  'manifest.md',
  'adapter-contract.md',
  'embedding.md',
  'lockfile.md',
  'architecture.md',
  'agents.md',
  'design/metering-agents-you-dont-control.md',
  'testing.md',
  'contributing.md',
  'release-process.md',
  'github-project-sync.md',
  'github-repository-audit-checklist.md',
  'RELEASE_NOTES.md',
];

export const docs = defineDocs({
  dir: '.',
  docs: {
    files,
  },
  meta: {
    files: ['meta.json'],
  },
});

export default defineConfig();
