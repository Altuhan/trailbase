import { initClient, type Client } from "trailbase";

import type { Config } from "./config";

/// Builds an authenticated TrailBase client from the environment config.
/// Pre-issued tokens take precedence over password login; with neither, the
/// client is anonymous and only world-accessible record APIs work.
export async function connect(config: Config): Promise<Client> {
  if (config.authToken) {
    return initClient(config.url, {
      tokens: {
        auth_token: config.authToken,
        refresh_token: config.refreshToken ?? null,
        csrf_token: null,
      },
    });
  }

  const client = initClient(config.url);
  if (config.user !== undefined && config.password !== undefined) {
    const mfa = await client.login(config.user, config.password);
    if (mfa !== undefined) {
      throw new Error(
        "Login requires a second factor. Headless use requires either a user without MFA or a pre-issued TRAILBASE_AUTH_TOKEN.",
      );
    }
  }
  return client;
}
