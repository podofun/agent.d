<script setup lang="ts">
import { withBase } from 'vitepress'
</script>

<template>
  <section class="agent-home">
    <div class="agent-home__grid" aria-hidden="true"></div>
    <div class="agent-home__inner">
      <div class="agent-home__copy">
        <p class="agent-home__eyebrow">podofun / agent.d</p>
        <h1>Local agents.<br />Explicit access.</h1>
        <p class="agent-home__lead">
          A Lua-scriptable runtime for tools, runners, services, memory, and permissions in one daemon.
        </p>
        <div class="agent-home__actions">
          <a class="agent-home__button agent-home__button--primary" href="https://github.com/podofun/agent.d">View on GitHub</a>
          <a class="agent-home__button agent-home__button--secondary" href="https://github.com/podofun">podofun org</a>
        </div>
      </div>

      <div class="agent-home__mark" aria-label="agent.d logo">
        <img :src="withBase('/agentd_logo.png')" alt="agent.d" />
      </div>
    </div>

    <div class="agent-home__features">
      <article>
        <p>Runtime</p>
        <h2>One Lua entry point</h2>
        <span>Load tools, runners, skills, services, and packages from a single daemon process.</span>
      </article>
      <article>
        <p>Access</p>
        <h2>Default-deny grants</h2>
        <span>Shell, filesystem, network, secrets, memory, and model calls stay behind explicit permissions.</span>
      </article>
      <article>
        <p>Control</p>
        <h2>Operator approvals</h2>
        <span>Inspect runtime state, follow traces, call actions, and resolve escalations from agentctl.</span>
      </article>
    </div>
  </section>
</template>
