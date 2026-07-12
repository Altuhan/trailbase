# Deploying the embedded MCP server

How to expose a production TrailBase instance to AI agents via `/mcp`
(Streamable HTTP), plus the stdio-over-SSH alternative. Commands assume a
Linux host with the `trail` binary at `/usr/local/bin/trail` and the depot
at `/srv/trailbase/traildepot`.

## 1. Dedicated agent user

Give agents their own non-admin account and grant it only the record APIs
it needs (narrowest ACL flags, optionally row-level rules):

```bash
trail --data-dir /srv/trailbase/traildepot user add agent@example.com "$(openssl rand -base64 24)"
```

## 2. systemd unit

```ini
# /etc/systemd/system/trailbase.service
[Unit]
Description=TrailBase
After=network.target

[Service]
User=trailbase
WorkingDirectory=/srv/trailbase
ExecStart=/usr/local/bin/trail --data-dir /srv/trailbase/traildepot run \
    --address 127.0.0.1:4000 \
    --mcp \
    --mcp-mode records \
    --mcp-redact-columns password,token,.*_secret \
    --mcp-allowed-hosts api.example.com
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Notes:

- `--mcp-allowed-hosts` must list the public hostname (DNS-rebinding
  protection allows only loopback otherwise).
- Start with `--mcp-mode read-only` if agents shouldn't write yet; budgets
  and two-phase confirmation stay on by default in `records` mode.

## 3. Reverse proxy

`/mcp` uses Server-Sent Events: disable buffering and allow long reads.

Caddy:

```
api.example.com {
    reverse_proxy 127.0.0.1:4000 {
        flush_interval -1
    }
}
```

nginx:

```nginx
location /mcp {
    proxy_pass http://127.0.0.1:4000;
    proxy_http_version 1.1;
    proxy_buffering off;
    proxy_cache off;
    proxy_read_timeout 24h;
    proxy_set_header Host $host;
    proxy_set_header Connection "";
}
```

## 4. Issue a token and connect

```bash
curl -s https://api.example.com/api/auth/v1/login \
  -H 'Content-Type: application/json' \
  -d '{"email": "agent@example.com", "password": "…"}'
# -> { "auth_token": "…", "refresh_token": "…", … }

claude mcp add --transport http trailbase https://api.example.com/mcp \
  --header "Authorization: Bearer <auth_token>"
```

Every tool call runs under that token's account and ACLs — different
clients with different tokens get different access, enforced server-side.

## 5. Token TTL — read this

TrailBase's default `auth.auth_token_ttl_sec` is **120 seconds**; regular
client libraries refresh transparently, but an MCP client sends a _static_
`Authorization` header, so tool calls start failing with 401-style errors
once the token expires. Options, in order of preference:

1. Raise the TTL in `config.textproto` (admin UI → Settings → Auth) to
   something operationally sane for agent use, e.g. `86400` (1 day), and
   re-issue tokens as part of starting an agent session.
2. Use the stdio transport instead (below) — it logs in itself at startup.

A refresh-aware `/mcp` (accepting the refresh token or an OAuth flow) is
the natural follow-up; until then, treat remote tokens as session-scoped
and short-lived.

## 6. Alternative: stdio over SSH (also enables the sandbox)

The sandbox (`--sandbox`) spawns child processes and is only available on
stdio. From your workstation:

```jsonc
// .mcp.json
{
  "mcpServers": {
    "trailbase": {
      "command": "ssh",
      "args": [
        "user@host",
        "TRAIL_MCP_PASSWORD=$(cat /srv/trailbase/agent-password)",
        "trail",
        "--data-dir",
        "/srv/trailbase/traildepot",
        "mcp",
        "--user",
        "agent@example.com",
        "--mode",
        "records",
        "--sandbox",
      ],
    },
  },
}
```

## 7. Security checklist

- [ ] Agent user is non-admin with minimal record API ACLs.
- [ ] `--mcp-allowed-hosts` lists exactly your public hostname(s).
- [ ] `--mcp-mode` is the lowest tier that does the job (`read-only` first).
- [ ] `--mcp-redact-columns` covers credential-ish columns.
- [ ] Budgets/confirmation left on (defaults) for `records` mode.
- [ ] Agent activity reviewed in the admin UI's `_logs` (every tool call is
      logged like a normal HTTP request with the acting user's id).
