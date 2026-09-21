# RecurOS commands (for AI agents)

You can run the `ctx` CLI yourself whenever the user asks for something
RecurOS does. The user does not need to know these commands. Rules:

- Save only when the user explicitly asks. Never log progress or summaries.
- Ask the user before any delete, and say exactly what will be deleted.
- Tags like `c:7f2a` identify one claim; ideas are short lowercase names.
- Everything below works offline except `ctx sync`; `new`, `use` and
  `delete` also sync to the cloud when they can.

## Saving and finding knowledge

| The user wants to | Run |
|---|---|
| save a decision | `ctx save "<what>" -k decision -w "<why>"` |
| save a constraint / rejected idea / fact / open question / belief | same, with `-k constraint`, `rejected`, `fact`, `question` or `claim` |
| attach files or links | add `-r path/to/file.rs,https://...` |
| save to the research branch instead of this repo's branch | add `--to research` |
| replace an old decision (history is kept) | `ctx save "<new>" -k decision --supersedes c:xxxx` |
| find something | `ctx search <words>` |
| see what is recorded | `ctx log` (add `-v` for reasons, `--all` to include deleted) |
| see one claim in full | `ctx show c:xxxx` |
| see what agents are given | `ctx pack` |

## Ideas, specs and building

| The user wants to | Run |
|---|---|
| start a new idea (chats will save into it) | `ctx new <idea>` |
| switch which idea chats save into | `ctx use <idea>` |
| save a spec from a file | `ctx spec save path/to/spec.md --to <idea>/research` |
| read the spec | `ctx spec show` (it is also in `.ctx/SPEC.md`) |
| list documents | `ctx spec ls` |
| turn an idea into a code repo | `ctx build <idea>` (creates `./<idea>` with `.ctx/SPEC.md` and `AGENTS.md`) |
| wire the current repo to an idea | `ctx init --project <idea>` |
| fill an existing repo's context from its own code | `ctx onboard` (prints a prompt; follow it yourself) |
| rename an idea, keeping its context | `ctx rename <old> <new>` |
| see every idea and its size | `ctx list` (`-a` also shows deleted ones) |
| delete a whole idea everywhere | `ctx delete <idea> --cloud` (ask first) |

## Sharing and viewing

| The user wants to | Run |
|---|---|
| a handoff document for a person | `ctx pack --for handoff > HANDOFF.md` |
| a picture of the project | `ctx map --out MAP.md` (Mermaid; renders on GitHub) |
| context to paste into another chat | `ctx pack --for dossier --clip` |
| context focused on one task | `ctx pack --task "<task>"` |

## Local node and self-hosted relay

| The user wants to | Run |
|---|---|
| initialise a self-hosted relay | `ctx relay init --data ./recuros-relay` |
| use MongoDB for relay metadata | set `MONGODB_URL` before every relay command (optional `MONGODB_DATABASE`; SQLite is the durable default) |
| run that relay | `ctx relay serve --data ./recuros-relay --addr 0.0.0.0:8788` |
| see or revoke a paired device | `ctx relay device ls` / `ctx relay device revoke <id>` |
| pair this device with it | `ctx node login --relay https://ctx.example.com --code <bootstrap-code>` |
| make local context available to chat MCP clients | `ctx node start` (uses a Quick Tunnel; use `--public-url https://...` for ngrok/named tunnels) |
| see this device's pairing | `ctx node status` |

The relay holds only device identity and live routing state. Claims and
documents stay in the local `ctx` store. Keep the relay behind HTTPS before
giving its MCP URL to a chat connector.

## Deleting (always ask first)

| The user wants to | Run |
|---|---|
| delete one claim | `ctx delete c:xxxx` |
| delete a whole idea (claims, spec, branches) | `ctx delete <idea> --yes` |
| delete one branch of an idea | `ctx delete <idea>/<branch> --yes` |
| make sure the chats see the delete too | add `--cloud` (fails if the cloud can't be reached) |

Deleted things disappear from packs, search, `AGENTS.md` and the chat
connectors, but stay in the history.

## Housekeeping

| The user wants to | Run |
|---|---|
| sync with other machines and the chat connectors | `ctx sync` |
| check that everything is set up | `ctx doctor` |
| where am I / what is pending | `ctx status` |
| review claims proposed for another branch | `ctx review`, then `ctx review accept c:xxxx` or `ctx review reject c:xxxx --reason "..."` |
| rate a claim as useful or misleading | `ctx rate c:xxxx up` / `down` |
| find near-duplicate claims | `ctx refine` |
| list branches | `ctx branch ls` |
