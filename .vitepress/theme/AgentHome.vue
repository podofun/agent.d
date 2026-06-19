<script setup lang="ts">
import { withBase } from 'vitepress'
</script>

<template>
  <section class="agent-home">
    <div class="agent-home__grid" aria-hidden="true"></div>
    <div class="agent-home__inner">
      <div class="agent-home__copy">
        <p class="agent-home__eyebrow">podofun / agent.d</p>
        <div class="agent-home__headline">
          <div class="agent-home__mark agent-home__mark--inline" aria-hidden="true">
            <img :src="withBase('/agentd_logo.png')" alt="" />
          </div>
          <h1>Local agents.<br />Explicit access.</h1>
        </div>
        <p class="agent-home__lead">
          A Lua-scriptable runtime for tools, runners, services, memory, and permissions in one daemon.
        </p>
        <div class="agent-home__actions">
          <a class="agent-home__button agent-home__button--primary" :href="withBase('/v0/guide/quick-start')">Get started</a>
          <a class="agent-home__button agent-home__button--secondary" :href="withBase('/v0/guide/what-is-agentd')">Read the guide</a>
        </div>
      </div>

      <div class="agent-home__demo" role="img" aria-label="Terminal: agentctl call git.status checks the shell.exec:git grant and returns a clean status.">
        <pre class="agent-home__demo-body" aria-hidden="true"><code><span class="c-pr">$</span> <span class="type">agentctl call <span class="c-fn">git.status</span></span>
<span class="out"><span class="c-mut"># checking grants.toml</span>
<span class="c-ok">✓</span> <span class="c-perm">shell.exec:git</span> <span class="c-mut">granted</span>
<span class="c-key">result</span> {
  <span class="c-key">status</span>: <span class="c-str">"clean"</span>,
  <span class="c-key">exit_code</span>: <span class="c-num">0</span>
}
<span class="c-pr">$</span> <span class="agent-home__caret"></span></span></code></pre>
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
        <h2>Explicit grants</h2>
        <span>Shell, filesystem, network, secrets, memory, and model calls stay behind permissions you grant.</span>
      </article>
      <article>
        <p>Control</p>
        <h2>Operator approvals</h2>
        <span>Inspect runtime state, follow traces, call actions, and resolve escalations from agentctl.</span>
      </article>
    </div>
  </section>
</template>
