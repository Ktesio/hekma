import { remarkMdxMermaid } from 'fumadocs-core/mdx-plugins';
import { defineConfig, defineDocs } from 'fumadocs-mdx/config';



const files = [
  'README.md',
  'get-started.md',
  'installation.md',
  'migration.md',
  'troubleshooting.md',
  'commands.md',
  'control-api.md',
  'hosting.md',
  'manifest.md',
  'adapter-contract.md',
  'embedding.md',
  'lockfile.md',
  'architecture.md',
  'agents.md',
  'adapters.md',
  'adapters/hermes.md',
  'adapters/opencode.md',
  'adapters/gemini-cli.md',
  'adapters/copilot-cli.md',
  'adapters/cursor-cli.md',
  'adapters/goose.md',
  'adapters/claude-code.md',
  'adapters/codex.md',
  'adapters/crush.md',
  'adapters/aider.md',
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

export default defineConfig({
  // fumadocs-core's official converter: mermaid fences compile to
  // `<Mermaid chart="…" />`, rendered client-side by the component mapped
  // in components/mdx.tsx (docs/components/mermaid.tsx).
  mdxOptions: {
    remarkPlugins: [remarkMdxMermaid],
  },
});
