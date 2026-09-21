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

`ctx node start` opens an outbound authenticated connection, so the computer
does not need an inbound port, a Cloudflare account, GitHub, or a tunnel URL.
Stop it and remote MCP calls return an explicit offline error.

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
