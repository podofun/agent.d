import { defineConfig } from 'vitepress'

export default defineConfig({
  base: '/agentd/',
  srcDir: 'docs',
  srcExclude: [
    'api-examples.md',
    'markdown-examples.md',
    'superpowers/**'
  ],
  title: 'agent.d',
  description: 'Local agents with explicit access.',
  vite: {
    publicDir: '../assets'
  },
  themeConfig: {
    nav: [
      { text: 'Home', link: '/' },
      { text: 'GitHub', link: 'https://github.com/podofun/agent.d' }
    ],

    sidebar: [],

    socialLinks: [
      { icon: 'github', link: 'https://github.com/podofun/agent.d' }
    ],

    search: {
      provider: 'local'
    }
  }
})
