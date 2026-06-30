// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// Deployed to GitHub Pages at https://clement-tourriere.github.io/devflow/
export default defineConfig({
  site: 'https://clement-tourriere.github.io',
  base: '/devflow',
  integrations: [
    starlight({
      title: 'devflow',
      description:
        'Isolated development environments for every Git workspace — worktrees, databases, caches, and stateful services that sync with your Git workflow.',
      favicon: '/favicon.png',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/clement-tourriere/devflow',
        },
      ],
      editLink: {
        baseUrl: 'https://github.com/clement-tourriere/devflow/edit/main/docs-site/',
      },
      customCss: ['./src/styles/custom.css'],
      sidebar: [
        {
          label: 'Getting started',
          items: [
            'getting-started/installation',
            'getting-started/quickstart',
            'getting-started/shell-integration',
            'getting-started/existing-project',
          ],
        },
        {
          label: 'Concepts',
          items: [
            'concepts/workspaces',
            'concepts/worktrees',
            'concepts/services',
            'concepts/hooks',
          ],
        },
        {
          label: 'Guides',
          items: [
            'guides/worktrees',
            'guides/local-containers',
            'guides/shared-engines',
            'guides/seeding',
            'guides/hooks',
            'guides/processes',
            'guides/proxy',
            'guides/ai-agents',
            'guides/merging',
            'guides/cloud-providers',
            'guides/plugins',
            'guides/gui',
            'guides/tui',
          ],
        },
        {
          label: 'Reference',
          items: [
            'reference/cli',
            'reference/configuration',
            'reference/hooks',
            'reference/environment',
          ],
        },
        'troubleshooting',
      ],
    }),
  ],
});
