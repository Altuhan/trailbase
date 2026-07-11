# TrailBase Examples

#### [Blog](blog/)

A simple styled Blog example with UIs both for web and Flutter:

<p align="center">
  <picture align="center">
    <img
      height="420"
      src="blog/screenshots/screenshot_web.png"
      alt="Blog web app"
    />
  </picture>

  <picture align="center">
    <img
      height="420"
      src="blog/screenshots/screenshot_flutter.png"
      alt="Blog Flutter app"
    />
  </picture>
</p>

#### [TanStack/db Synced Todo Example](tanstack-db-sync/)

A simple ToDo App demonstrating the use of
[TanStack/db](https://github.com/TanStack/db) to automatically sync
items and settings across tabs, Browsers and devices.

<p align="center">
  <picture align="center">
    <img
      height="340"
      src="tanstack-db-sync/screenshots/screenshot0.png"
      alt="ToDo example app using TanStack/db for cross-device sync"
    />
  </picture>
</p>

#### [Server-Side Rendered Collaborative Clicker](collab-clicker-ssr/)

A small clicker application, where we can collaboratively make it go 🚀. It
show-cases server-side rendering using SolidJS, however it could equally be
React, Vue, Svelte, Preact, ... . After client-side hydration, click counter
changes are streamed to everyone listening.

<p align="center">
  <picture align="center">
     <img
      height="400"
      src="collab-clicker-ssr/screenshots/screenshot0.png"
      alt="Collaborative acorn clicker"
    />
  </picture>
</p>

#### [Coffee Vector Search](coffee-vector-search/)

A small single-page web app demonstrating vector search and custom JS/TS endpoints.

<p align="center">
  <picture align="center">
    <img
      height="420"
      src="coffee-vector-search/screenshots/screenshot0.png"
      alt="Coffee vector search web app"
    />
  </picture>
</p>

#### [Data CLI App](data-cli-tutorial/)

A brief example of how TrailBase can be used in an command-line app to ingest
IMDB data and query it. This code belongs to the
[CLI tutorial](https://trailbase.io/getting-started/first-cli-app):

#### [Custom Rust Binary](custom-binary/)

A quick example showcasing how one can use TrailBase as a library.

#### [MCP Server](mcp-server/)

An [MCP](https://modelcontextprotocol.io) server giving AI agents (Claude Code,
Claude Desktop, …) safe, mode-gated access to a TrailBase instance: Record API
CRUD as a dedicated non-admin user for live data, plus an ephemeral "production
sandbox" — a throwaway snapshot of the live depot where schema changes are made
and handed back as reviewable migration files instead of touching production.
