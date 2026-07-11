import { defineConfig } from "vite";
import { resolve } from "node:path";

// Bundles the workspace `trailbase` client (whose dev entry point is
// TypeScript source) into the output so `node dist/index.js` works without a
// build step in the client package. Published dependencies stay external.
export default defineConfig({
  build: {
    outDir: "./dist",
    minify: false,
    target: "node22",
    lib: {
      entry: resolve(__dirname, "src/index.ts"),
      fileName: "index",
      formats: ["es"],
    },
    rollupOptions: {
      external: [
        /^node:/,
        "@modelcontextprotocol/sdk",
        /^@modelcontextprotocol\/sdk\//,
        "zod",
        "nano-spawn",
      ],
    },
  },
});
