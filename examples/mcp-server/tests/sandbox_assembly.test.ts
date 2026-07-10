import {
  mkdtempSync,
  mkdirSync,
  rmSync,
  writeFileSync,
  existsSync,
  readFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { afterEach, beforeEach, describe, expect, test } from "vitest";

import { assembleSandboxDepot, snapshotSqliteDb } from "../src/sandbox";

let sourceDir: string;
let sandboxDir: string;

function createSourceDepot(dir: string): void {
  mkdirSync(join(dir, "data"), { recursive: true });
  mkdirSync(join(dir, "migrations", "main"), { recursive: true });
  mkdirSync(join(dir, "secrets"), { recursive: true });

  const db = new DatabaseSync(join(dir, "data", "main.db"));
  db.exec("CREATE TABLE article (id INTEGER PRIMARY KEY, title TEXT NOT NULL)");
  db.exec("INSERT INTO article (title) VALUES ('hello'), ('world')");
  db.close();

  writeFileSync(
    join(dir, "config.textproto"),
    'email { sender_name: "Test" }\n',
  );
  writeFileSync(
    join(dir, "migrations", "main", "U1__create_article.sql"),
    "CREATE TABLE article (id INTEGER PRIMARY KEY, title TEXT NOT NULL);\n",
  );
  writeFileSync(join(dir, "secrets", "PRIVATE"), "do-not-copy");
}

beforeEach(() => {
  sourceDir = mkdtempSync(join(tmpdir(), "tb-mcp-src-"));
  sandboxDir = mkdtempSync(join(tmpdir(), "tb-mcp-sb-"));
  createSourceDepot(sourceDir);
});

afterEach(() => {
  rmSync(sourceDir, { recursive: true, force: true });
  rmSync(sandboxDir, { recursive: true, force: true });
});

describe("snapshotSqliteDb", () => {
  test("produces a consistent, standalone copy", async () => {
    const dest = join(sandboxDir, "copy.db");
    await snapshotSqliteDb(join(sourceDir, "data", "main.db"), dest);

    const db = new DatabaseSync(dest, { readOnly: true });
    try {
      const rows = db.prepare("SELECT title FROM article ORDER BY id").all();
      expect(rows).toEqual([{ title: "hello" }, { title: "world" }]);
    } finally {
      db.close();
    }
  });
});

describe("assembleSandboxDepot", () => {
  test("copies data snapshot, config and migrations but not secrets", async () => {
    const { baselineMigrations, baselineConfig } = await assembleSandboxDepot(
      sourceDir,
      sandboxDir,
    );

    expect(existsSync(join(sandboxDir, "data", "main.db"))).toBe(true);
    expect(
      readFileSync(join(sandboxDir, "config.textproto"), "utf8"),
    ).toContain("sender_name");
    expect(
      existsSync(
        join(sandboxDir, "migrations", "main", "U1__create_article.sql"),
      ),
    ).toBe(true);
    // Fresh keys/sessions by design: secrets must never be copied.
    expect(existsSync(join(sandboxDir, "secrets"))).toBe(false);

    expect(baselineMigrations).toEqual(["U1__create_article.sql"]);
    expect(baselineConfig).toContain("sender_name");

    const db = new DatabaseSync(join(sandboxDir, "data", "main.db"), {
      readOnly: true,
    });
    try {
      const row = db.prepare("SELECT COUNT(*) AS n FROM article").get();
      expect(row).toEqual({ n: 2 });
    } finally {
      db.close();
    }
  });

  test("rejects a directory that is not a depot", async () => {
    const empty = mkdtempSync(join(tmpdir(), "tb-mcp-empty-"));
    try {
      await expect(assembleSandboxDepot(empty, sandboxDir)).rejects.toThrow(
        /does not exist/,
      );
    } finally {
      rmSync(empty, { recursive: true, force: true });
    }
  });
});
