import {
  GetConfigResponse,
  UpdateConfigRequest,
} from "../../../crates/assets/js/admin/proto/config_api";
import type { Config as TrailBaseConfig } from "../../../crates/assets/js/admin/proto/config";

/// Extracts the CSRF token from an auth JWT's claims. Admin endpoints require
/// a `CSRF-Token` header matching the token's `csrf_token` claim.
export function csrfFromJwt(token: string): string | undefined {
  const parts = token.split(".");
  if (parts.length !== 3) {
    return undefined;
  }
  try {
    const claims: unknown = JSON.parse(
      Buffer.from(parts[1], "base64url").toString("utf8"),
    );
    if (
      typeof claims === "object" &&
      claims !== null &&
      "csrf_token" in claims &&
      typeof claims.csrf_token === "string"
    ) {
      return claims.csrf_token;
    }
  } catch {
    // Fall through.
  }
  return undefined;
}

export class AdminApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(`Admin API error (HTTP ${status}): ${message}`);
  }
}

/// Minimal typed client for TrailBase's `/api/_admin` endpoints. The admin UI
/// has its own internal client; this stays dependency-light and only covers
/// what the MCP tools need.
export class AdminClient {
  private constructor(
    private readonly base: string,
    private readonly token: string,
    private readonly csrf: string | undefined,
  ) {}

  static fromToken(url: string, token: string): AdminClient {
    return new AdminClient(url.replace(/\/$/, ""), token, csrfFromJwt(token));
  }

  private headers(): Record<string, string> {
    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.token}`,
    };
    if (this.csrf !== undefined) {
      headers["CSRF-Token"] = this.csrf;
    }
    return headers;
  }

  private async request(
    method: "GET" | "POST" | "PATCH" | "DELETE",
    path: string,
    body?: BodyInit,
    contentType?: string,
  ): Promise<Response> {
    const headers = this.headers();
    if (contentType !== undefined) {
      headers["Content-Type"] = contentType;
    }
    const response = await fetch(`${this.base}/api/_admin${path}`, {
      method,
      headers,
      body,
    });
    if (!response.ok) {
      throw new AdminApiError(response.status, await response.text());
    }
    return response;
  }

  async getJson<T = unknown>(path: string): Promise<T> {
    const response = await this.request("GET", path);
    return (await response.json()) as T;
  }

  async sendJson<T = unknown>(
    method: "POST" | "PATCH" | "DELETE",
    path: string,
    body: unknown,
  ): Promise<T> {
    const response = await this.request(
      method,
      path,
      JSON.stringify(body),
      "application/json",
    );
    const text = await response.text();
    return (text.length > 0 ? JSON.parse(text) : null) as T;
  }

  // Typed helpers used by the MCP tools.

  tables(): Promise<unknown> {
    return this.getJson("/tables");
  }

  info(): Promise<unknown> {
    return this.getJson("/info");
  }

  jobs(): Promise<unknown> {
    return this.getJson("/jobs");
  }

  runJob(id: number): Promise<unknown> {
    return this.sendJson("POST", "/job/run", { id });
  }

  /// `query` is a raw trailbase-qs query string, e.g.
  /// `limit=50&order=-created&filter[status][$gte]=400`.
  logs(query?: string): Promise<unknown> {
    return this.getJson(`/logs/list${query ? `?${query}` : ""}`);
  }

  /// Returns the (secret-redacted) server config as proto-JSON plus the hash
  /// required for optimistic concurrency on updates.
  async getConfig(): Promise<{ config: unknown; hash: string | undefined }> {
    const response = await this.request("GET", "/config");
    const message = GetConfigResponse.decode(
      new Uint8Array(await response.arrayBuffer()),
    );
    const json = GetConfigResponse.toJSON(message) as {
      config?: unknown;
      hash?: string;
    };
    return { config: json.config, hash: json.hash };
  }

  /// Replaces the server config. `config` is proto-JSON in the same shape
  /// returned by `getConfig()`; `hash` must be the value from `getConfig()`.
  async updateConfig(config: unknown, hash: string): Promise<void> {
    const message = UpdateConfigRequest.fromJSON({ config, hash });
    const body = UpdateConfigRequest.encode(message).finish();
    await this.request(
      "POST",
      "/config",
      body as BodyInit,
      "application/octet-stream",
    );
  }

  /// Executes arbitrary SQL on the instance (writer connection!). Only ever
  /// exposed for sandbox instances.
  execQuery(query: string): Promise<unknown> {
    return this.sendJson("POST", "/query", { query });
  }
}

export type { TrailBaseConfig };
