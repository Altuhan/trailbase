# Containerized TrailBase MCP server (stdio transport).
#
# Build FROM THE REPOSITORY ROOT (the workspace client sources are needed):
#   docker build -f examples/mcp-server/Dockerfile -t trailbase-mcp .
#
# Run with an MCP client, e.g. Claude Code:
#   claude mcp add trailbase -- docker run -i --rm \
#     -e TRAILBASE_URL=https://your-instance.example \
#     -e TRAILBASE_MODE=prod-safe \
#     -e TRAILBASE_USER=claude-agent@example.com \
#     -e TRAILBASE_PASSWORD=... \
#     trailbase-mcp
#
# Note: `-i` is required — MCP talks over stdin/stdout. Sandbox mode inside a
# container additionally needs the depot and a `trail` binary mounted in
# (-v /srv/traildepot:/depot -e TRAILBASE_DATA_DIR=/depot -e TRAIL_BIN=...).

FROM node:22-slim AS build
RUN corepack enable
WORKDIR /repo

COPY pnpm-workspace.yaml pnpm-lock.yaml .npmrc ./
COPY crates/assets/js/client crates/assets/js/client
COPY crates/assets/js/admin/proto crates/assets/js/admin/proto
COPY examples/mcp-server examples/mcp-server

# The filtered install only needs the mcp-server subtree; stub out the other
# workspace projects referenced by pnpm-workspace.yaml so pnpm doesn't fail
# on missing directories.
RUN node -e ' \
  const fs = require("node:fs"); \
  const yaml = fs.readFileSync("pnpm-workspace.yaml", "utf8"); \
  for (const m of yaml.matchAll(/^\s+-\s+\x27([^\x27]+)\x27/gm)) { \
    const dir = m[1]; \
    if (!fs.existsSync(dir)) { \
      fs.mkdirSync(dir, { recursive: true }); \
      fs.writeFileSync(dir + "/package.json", JSON.stringify({ name: "stub-" + dir.replaceAll("/", "-"), private: true, version: "0.0.0" })); \
    } \
  }'
RUN pnpm install --filter trailbase-mcp... --frozen-lockfile=false
RUN pnpm -C examples/mcp-server build

# Stage a plain-npm production install so the runtime image needs no pnpm.
RUN mkdir /stage && cp examples/mcp-server/dist/index.js /stage/ && \
    node -e ' \
      const fs = require("node:fs"); \
      const deps = {}; \
      for (const name of ["@bufbuild/protobuf", "@modelcontextprotocol/sdk", "nano-spawn", "zod"]) { \
        deps[name] = JSON.parse(fs.readFileSync("examples/mcp-server/node_modules/" + name + "/package.json", "utf8")).version; \
      } \
      fs.writeFileSync("/stage/package.json", JSON.stringify({ name: "trailbase-mcp", private: true, type: "module", dependencies: deps })); \
    ' && \
    cd /stage && npm install --omit=dev --no-audit --no-fund --loglevel=error

FROM node:22-slim
LABEL org.opencontainers.image.title="trailbase-mcp"
LABEL org.opencontainers.image.description="MCP server for safe agent access to a TrailBase instance"
WORKDIR /app
COPY --from=build /stage /app
USER node
ENTRYPOINT ["node", "/app/index.js"]
