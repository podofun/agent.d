import { defineConfig } from 'vitepress'

export default defineConfig({
  base: '/agentd/',
  outDir: '.vitepress/dist/agentd',
  srcDir: 'docs',
  srcExclude: [
    'api-examples.md',
    'markdown-examples.md',
    'superpowers/**'
  ],
  title: 'agent.d',
  description: 'A portable runtime for tool-using AI agents.',
  cleanUrls: true,
  lastUpdated: true,
  vite: {
    publicDir: '../assets'
  },
  themeConfig: {
    logo: '/agentd_logo.png',

    nav: [
      { text: 'Guide', link: '/v0/guide/what-is-agentd', activeMatch: '^/v0/(guide|tutorial|concepts|writing)/' },
      { text: 'Reference', link: '/v0/reference/ctx/', activeMatch: '^/v0/(reference|security|providers|packages)/' },
      { text: 'Recipes', link: '/v0/recipes/', activeMatch: '^/v0/recipes/' },
      { text: 'Operations', link: '/v0/operations/deployment', activeMatch: '^/v0/operations/' },
      {
        // Version switcher. Add future versions here as they ship.
        text: 'v0',
        items: [
          { text: 'v0 (current)', link: '/v0/guide/what-is-agentd' }
        ]
      },
      { text: 'GitHub', link: 'https://github.com/podofun/agent.d' }
    ],

    sidebar: {
      '/v0/': [
        {
          text: 'Introduction',
          items: [
            { text: 'What is agent.d?', link: '/v0/guide/what-is-agentd' },
            { text: 'How it works', link: '/v0/guide/how-it-works' },
            { text: 'Installation', link: '/v0/guide/installation' },
            { text: 'Quick start', link: '/v0/guide/quick-start' }
          ]
        },
        {
          text: 'Tutorial: Build Your First Agent',
          collapsed: false,
          items: [
            { text: 'Overview', link: '/v0/tutorial/' },
            { text: '1. Configuration directory', link: '/v0/tutorial/config-directory' },
            { text: '2. Git action', link: '/v0/tutorial/first-tool' },
            { text: '3. Permissions', link: '/v0/tutorial/permissions' },
            { text: '4. Runner and skill', link: '/v0/tutorial/runner-and-skill' },
            { text: '5. Run the agent', link: '/v0/tutorial/calling' }
          ]
        },
        {
          text: 'Core Concepts',
          collapsed: false,
          items: [
            { text: 'Overview', link: '/v0/concepts/' },
            { text: 'The runtime', link: '/v0/concepts/runtime' },
            { text: 'Tools & actions', link: '/v0/concepts/tools-and-actions' },
            { text: 'Runners', link: '/v0/concepts/runners' },
            { text: 'Skills', link: '/v0/concepts/skills' },
            { text: 'Services', link: '/v0/concepts/services' },
            { text: 'Memory & state', link: '/v0/concepts/memory-and-state' },
            { text: 'Interfaces & callers', link: '/v0/concepts/interfaces-and-callers' },
            { text: 'The permission model', link: '/v0/concepts/permissions' }
          ]
        },
        {
          text: 'Writing Components',
          collapsed: true,
          items: [
            { text: 'init.lua & imports', link: '/v0/writing/init' },
            { text: 'Tools & actions', link: '/v0/writing/tools' },
            { text: 'Runners', link: '/v0/writing/runners' },
            { text: 'Skills', link: '/v0/writing/skills' },
            { text: 'Services', link: '/v0/writing/services' },
            { text: 'The ctx handle', link: '/v0/writing/context' }
          ]
        },
        {
          text: 'Capability Reference',
          collapsed: true,
          items: [
            { text: 'Overview', link: '/v0/reference/ctx/' },
            { text: 'ctx.log', link: '/v0/reference/ctx/logging' },
            { text: 'ctx.shell', link: '/v0/reference/ctx/shell' },
            { text: 'ctx.fs', link: '/v0/reference/ctx/fs' },
            { text: 'ctx.http', link: '/v0/reference/ctx/http' },
            { text: 'ctx.ws', link: '/v0/reference/ctx/websocket' },
            { text: 'ctx.secret', link: '/v0/reference/ctx/secrets' },
            { text: 'ctx.memory & ctx.state', link: '/v0/reference/ctx/memory' },
            { text: 'ctx.ai', link: '/v0/reference/ctx/ai' },
            { text: 'ctx.call / run / structured', link: '/v0/reference/ctx/calls' },
            { text: 'ctx.caller', link: '/v0/reference/ctx/caller' },
            { text: 'Concurrency & timers', link: '/v0/reference/ctx/concurrency' },
            { text: 'Standard library', link: '/v0/reference/ctx/stdlib' }
          ]
        },
        {
          text: 'Permissions & Security',
          collapsed: true,
          items: [
            { text: 'grants.toml', link: '/v0/security/grants' },
            { text: 'Permission slugs', link: '/v0/security/permission-slugs' },
            { text: 'Interactive approvals', link: '/v0/security/approvals' },
            { text: 'The shell sandbox', link: '/v0/security/sandbox' },
            { text: 'Security best practices', link: '/v0/security/best-practices' }
          ]
        },
        {
          text: 'Models & Providers',
          collapsed: true,
          items: [
            { text: 'Overview', link: '/v0/providers/' },
            { text: 'Anthropic', link: '/v0/providers/anthropic' },
            { text: 'OpenAI', link: '/v0/providers/openai' },
            { text: 'CLI backends', link: '/v0/providers/cli-backends' },
            { text: 'Codex app-server', link: '/v0/providers/codex' },
            { text: 'Credentials', link: '/v0/providers/credentials' },
            { text: 'MCP integration', link: '/v0/providers/mcp' }
          ]
        },
        {
          text: 'Packages',
          collapsed: true,
          items: [
            { text: 'Using packages', link: '/v0/packages/' },
            { text: 'Authoring a package', link: '/v0/packages/authoring' },
            { text: 'Managing packages', link: '/v0/packages/managing' }
          ]
        },
        {
          text: 'Reference',
          collapsed: true,
          items: [
            { text: 'agentctl CLI', link: '/v0/reference/cli' },
            { text: 'WebSocket protocol', link: '/v0/reference/protocol' },
            { text: 'Configuration', link: '/v0/reference/configuration' },
            { text: 'Glossary', link: '/v0/reference/glossary' }
          ]
        },
        {
          text: 'Operations',
          collapsed: true,
          items: [
            { text: 'Deployment', link: '/v0/operations/deployment' },
            { text: 'Observability', link: '/v0/operations/observability' },
            { text: 'Troubleshooting', link: '/v0/operations/troubleshooting' }
          ]
        },
        {
          text: 'Recipes',
          collapsed: true,
          items: [
            { text: 'Overview', link: '/v0/recipes/' },
            { text: 'Code-review runner', link: '/v0/recipes/code-review' },
            { text: 'Discord bot', link: '/v0/recipes/discord-bot' },
            { text: 'HTTP API tool', link: '/v0/recipes/http-tool' },
            { text: 'Webhook-triggered action', link: '/v0/recipes/webhook' },
            { text: 'Per-user memory', link: '/v0/recipes/per-user-memory' }
          ]
        },
        {
          text: 'Contributing',
          collapsed: true,
          items: [
            { text: 'Architecture', link: '/v0/contributing/architecture' }
          ]
        }
      ]
    },

    socialLinks: [
      { icon: 'github', link: 'https://github.com/podofun/agent.d' }
    ],

    editLink: {
      pattern: 'https://github.com/podofun/agent.d/edit/main/docs/:path',
      text: 'Edit this page on GitHub'
    },

    search: {
      provider: 'local'
    },

    footer: {
      message: 'Released under the MIT License.',
      copyright: 'Copyright © podofun / agent.d'
    }
  }
})
