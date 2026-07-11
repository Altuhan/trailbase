import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import { ConfigError, loadConfig, type Config } from "../src/config";
import { GuardError, Guards, redactData } from "../src/guards";
import { textResult } from "../src/tools/common";

function guardConfig(overrides: Partial<Config> = {}): Config {
  return {
    url: "http://localhost:4000",
    mode: "prod-safe",
    trailBin: "trail",
    budgetWrites: 100,
    redactColumns: [],
    confirmWrites: true,
    confirmTimeoutSecs: 120,
    ...overrides,
  };
}

describe("config", () => {
  test("guard defaults per mode", () => {
    const prod = loadConfig({});
    expect(prod.mode).toBe("prod-safe");
    expect(prod.confirmWrites).toBe(true);
    expect(prod.budgetWrites).toBe(100);

    const sandbox = loadConfig({ TRAILBASE_MODE: "sandbox" });
    expect(sandbox.confirmWrites).toBe(false);

    const overridden = loadConfig({
      TRAILBASE_MODE: "sandbox",
      TRAILBASE_CONFIRM_WRITES: "true",
    });
    expect(overridden.confirmWrites).toBe(true);
    expect(
      loadConfig({ TRAILBASE_CONFIRM_WRITES: "false" }).confirmWrites,
    ).toBe(false);
  });

  test("redact patterns parse case-insensitively", () => {
    const config = loadConfig({
      TRAILBASE_REDACT_COLUMNS: "password, .*_key ,,",
    });
    expect(config.redactColumns).toHaveLength(2);
    expect(config.redactColumns[0].test("user_PASSWORD")).toBe(true);
    expect(config.redactColumns[1].test("api_key")).toBe(true);
  });

  test("invalid redact pattern is rejected", () => {
    expect(() => loadConfig({ TRAILBASE_REDACT_COLUMNS: "([" })).toThrow(
      ConfigError,
    );
  });
});

describe("redactData", () => {
  const patterns = [/password/i, /^secret/i];

  test("masks matching columns, including nested and expanded rows", () => {
    const redacted = redactData(
      {
        records: [
          {
            id: 1,
            password_hash: "abc",
            author: { name: "x", secret_note: "y" },
          },
        ],
        cursor: "c",
      },
      patterns,
    );
    expect(redacted).toEqual({
      records: [
        {
          id: 1,
          password_hash: "[REDACTED]",
          author: { name: "x", secret_note: "[REDACTED]" },
        },
      ],
      cursor: "c",
    });
  });

  test("no patterns means identity", () => {
    const data = { password: "p" };
    expect(redactData(data, [])).toBe(data);
  });
});

describe("Guards", () => {
  const ok = () => Promise.resolve(textResult("OK"));

  test("inactive in sandbox mode", () => {
    const guards = new Guards(guardConfig({ mode: "sandbox" }));
    expect(guards.active).toBe(false);
    expect(guards.confirmWrites).toBe(false);
    expect(guards.budgetRemaining()).toBe(null);
  });

  test("budget exhaustion raises with a hint", () => {
    const guards = new Guards(
      guardConfig({ budgetWrites: 2, confirmWrites: false }),
    );
    guards.useBudget();
    guards.useBudget();
    expect(guards.budgetRemaining()).toBe(0);
    expect(() => guards.useBudget()).toThrow(/TRAILBASE_BUDGET_WRITES/);
  });

  test("budget 0 means unlimited", () => {
    const guards = new Guards(guardConfig({ budgetWrites: 0 }));
    expect(guards.budgetRemaining()).toBe(null);
    for (let i = 0; i < 500; i++) {
      guards.useBudget();
    }
  });

  test("propose/confirm executes once and charges budget", async () => {
    const guards = new Guards(guardConfig({ budgetWrites: 5 }));
    const execute = vi.fn(ok);
    const pending = guards.propose("records_create", "create x", execute);

    const confirmed = guards.confirm(pending.id);
    await confirmed.execute();
    expect(execute).toHaveBeenCalledTimes(1);
    expect(guards.budgetRemaining()).toBe(4);

    // A pending id is single-use.
    expect(() => guards.confirm(pending.id)).toThrow(GuardError);
  });

  test("cancel discards without charging", () => {
    const guards = new Guards(guardConfig({ budgetWrites: 5 }));
    const pending = guards.propose("records_delete", "delete x", ok);
    expect(guards.cancel(pending.id).summary).toBe("delete x");
    expect(guards.budgetRemaining()).toBe(5);
    expect(() => guards.cancel(pending.id)).toThrow(/No pending write/);
  });

  test("caps the number of unconfirmed writes", () => {
    const guards = new Guards(guardConfig());
    for (let i = 0; i < 10; i++) {
      guards.propose("records_create", `create ${i}`, ok);
    }
    expect(() => guards.propose("records_create", "one too many", ok)).toThrow(
      /Too many unconfirmed writes/,
    );
  });

  describe("expiry", () => {
    beforeEach(() => {
      vi.useFakeTimers();
    });
    afterEach(() => {
      vi.useRealTimers();
    });

    test("pending writes expire after the timeout", () => {
      const guards = new Guards(guardConfig({ confirmTimeoutSecs: 60 }));
      const pending = guards.propose("records_update", "update x", ok);

      vi.setSystemTime(Date.now() + 61_000);
      expect(() => guards.confirm(pending.id)).toThrow(/expired/);
    });
  });
});
