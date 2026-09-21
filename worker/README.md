# RecurOS local-first Cloudflare Worker relay

This deploys the stable public MCP endpoint while context stays on the device
that runs `ctx node start`.

```text
ChatGPT / claude.ai → Worker + Durable Object → temporary HTTPS tunnel → local ctx node
```

The Durable Object stores only a bootstrap-used flag, device public key,
current tunnel URL and expiry, plus the relay signing key. It never stores
claims, documents, project files, packs, or semantic indexes.

## Deploy

Install dependencies and log in to your Cloudflare account:

```sh
cd worker
npm ci
npx wrangler login
```

Generate two independent high-entropy secrets and set them. The bootstrap
code is used once to pair the first device; the connector secret is what an
MCP client sends to `/mcp`.

```sh
npx wrangler secret put BOOTSTRAP_CODE
npx wrangler secret put CTX_SECRET
npx wrangler deploy
```

For example, generate a value locally with `openssl rand -base64 32`; add
`rcb_` to the bootstrap value and `rcm_` to the connector value so they are
easy to identify. Do not reuse either one and do not place them in
`wrangler.toml`.

Wrangler prints an address such as `https://recuros.<account>.workers.dev`.
Pair the repository whose local context should be available:

```sh
ctx node login --relay https://recuros.<account>.workers.dev --code <BOOTSTRAP_CODE>
ctx node start
```

By default `ctx node start` launches a Cloudflare Quick Tunnel for its
loopback executor. If you already run a named Cloudflare Tunnel, ngrok, or
another HTTPS tunnel for port 8790, use:

```sh
ctx node start --public-url https://your-tunnel.example
```

Configure ChatGPT or another MCP client with
`https://recuros.<account>.workers.dev/mcp` and header
`Authorization: Bearer <CTX_SECRET>`. The compatibility path
`/mcp/<CTX_SECRET>` also works, but headers keep credentials out of URLs and
logs.

## Security model

- The device creates an Ed25519 key pair locally. Its public key is enrolled
  once with the bootstrap code.
- Every URL registration is signed by that device key and expires quickly.
- Every Worker-to-node request is signed by a Worker key stored in the Durable
  Object. The node checks its signature, timestamp, and one-use nonce before
  it runs an MCP tool.
- The connector secret and the device key serve different roles. A connector
  cannot register a node; a device key cannot call `/mcp`.

For a fully self-hosted relay, use [`../relay/README.md`](../relay/README.md)
instead. It supports a local SQLite control plane or MongoDB for that same
small metadata set. The MongoDB Atlas Data API reached end of life in 2025, so
the Worker intentionally uses Cloudflare Durable Object SQLite rather than an
unsupported direct MongoDB HTTP integration.

The older GitHub-backed Worker implementation remains in `src/index.ts` and
its tests for migration/reference only. `wrangler.toml` deploys
`src/local-relay.ts`; GitHub is not part of this local-first path.
