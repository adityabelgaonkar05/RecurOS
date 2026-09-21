# Self-hosted RecurOS relay

The relay gives chat MCP clients one stable URL while every claim, document,
pack and index remains in a local `ctx` node. It is not a context database.

## Start it

From this repository:

```sh
cd relay
docker compose run --rm --entrypoint ctx relay relay init --data /data
docker compose up -d
```

By default this uses the persistent SQLite file at `relay/data/relay.db`.
For a managed MongoDB instance (for example MongoDB Atlas), set these before
**every** relay command, including the first `init`:

```sh
export MONGODB_URL='mongodb+srv://USER:PASSWORD@cluster.example.mongodb.net/?retryWrites=true&w=majority'
export MONGODB_DATABASE='recuros_relay' # optional; this is the default
docker compose build
docker compose run --rm --entrypoint ctx relay relay init --data /data
docker compose up -d
```

`--mongodb-url` and `--mongodb-database` provide the same settings for direct
`ctx relay` use. MongoDB receives only relay metadata: hashed connector
secrets, enrolled device public keys, pairing/revocation state and labels.
It never receives claims, documents, packs, project files or semantic indexes.
Keep the URL in your deployment's secret manager or `.env` file; do not commit
it. Use the same MongoDB URL/database for `init`, `serve`, `token` and
`device` commands.

SQLite and MongoDB relay metadata are separate stores. Switching an already
running relay to a new MongoDB database creates a fresh relay identity: run
`init`, pair the local node again, and create/use a new connector secret.
Your local context data is unaffected.

The first command prints two secrets once:

- the bootstrap code, used once to pair the first local node;
- the connector secret, used as an MCP connector credential.

Put HTTPS in front of port 8788 before exposing it publicly. Configure an MCP
client with the public URL `https://ctx.example.com/mcp` and header
`Authorization: Bearer <connector-secret>`. The path form
`/mcp/<connector-secret>` is available only for clients that cannot send a
header; avoid it because URLs tend to be logged.

On the computer whose context should be served:

```sh
ctx node login --relay https://ctx.example.com --code <bootstrap-code>
ctx node start
```

`ctx node start` serves only `127.0.0.1:8790`, starts a Cloudflare Quick
Tunnel, and signs the generated URL into the relay every 30 seconds. The
relay signs every request before posting it through that URL; the local node
rejects unsigned, expired, or replayed requests. The computer needs no
inbound port, Cloudflare account, or GitHub token. Stop it and remote MCP
calls return an explicit offline error.

For a named Cloudflare Tunnel, ngrok, or another HTTPS tunnel, point it at
`127.0.0.1:8790` and pass its public address instead of starting Quick Tunnel:

```sh
ctx node start --public-url https://your-tunnel.example
```

The relay accepts only public HTTPS DNS hostnames for registrations. Never
expose the loopback executor directly.

Pairing from a repository makes its bound branch the remote default. To serve
a different branch later, use `ctx node start --branch project/branch`.

To create another revocable connector secret:

```sh
docker compose run --rm --entrypoint ctx relay relay token --data /data --label chatgpt
```

To inspect or revoke a paired device immediately:

```sh
docker compose run --rm --entrypoint ctx relay relay device ls --data /data
docker compose run --rm --entrypoint ctx relay relay device revoke <device-id> --data /data
```

The relay stores a hash of connector secrets, enrolled public device keys and
live routing state only. Back up `relay/data` when using SQLite, or back up
the selected MongoDB database when using MongoDB. Back up the local `ctx`
store separately because that is where context lives.
