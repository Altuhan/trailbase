import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";

import { loadConfig } from "./config";
import { enabledTiers } from "./policy";
import { connect } from "./trailbase";
import { newToolContext } from "./tools/common";
import { registerAllTools } from "./tools";

async function main(): Promise<void> {
  const config = loadConfig();

  const server = new McpServer({
    name: "trailbase-mcp",
    version: "0.1.0",
  });

  registerAllTools(server, newToolContext(config, connect));

  await server.connect(new StdioServerTransport());

  process.stderr.write(
    `trailbase-mcp: mode=${config.mode} url=${config.url} tiers=[${[...enabledTiers(config.mode)].join(", ")}]\n`,
  );
}

main().catch((err: unknown) => {
  process.stderr.write(`trailbase-mcp failed to start: ${err}\n`);
  process.exit(1);
});
