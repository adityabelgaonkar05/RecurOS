<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/recuros-light.svg">
  <img src="assets/recuros.svg" alt="RecurOS" width="72" height="72">
</picture>

# RecurOS

**One memory for all your AI tools.**

You have an idea in ChatGPT. You research it more in Claude. Then you open Claude Code to build it... and it knows nothing. You copy, paste and re-explain everything, every time.

RecurOS fixes that. You tell any AI **"save that"**, and every other AI you use remembers it: ChatGPT, claude.ai, Claude Code, Cursor, Codex. When the research is done, one command hands the whole plan to your coding agent.

```
 ChatGPT            claude.ai            Claude Code
 "save that"   →    "what do we     →    already knows the plan,
                     have so far?"        what you decided,
                                          and what you ruled out
```

- **Local-first.** Your context lives and is searched on your computer. A relay only reaches an online local node; it is not a context database.
- **Self-hostable.** Run that relay yourself with Docker, without a GitHub repository or token.
- **You stay in control.** Nothing is saved unless you ask, and you can delete anything.

---



## Contents

1. [A 2-minute example: building a notes app](#a-2-minute-example-building-a-notes-app)
2. [Install](#1-install)
3. [Use it with Claude Code (local only)](#2-use-it-with-claude-code-local-only)
4. [Self-host a local node relay](#self-host-a-local-node-relay)
5. [Connect ChatGPT and claude.ai through GitHub (legacy, optional)](#3-connect-chatgpt-and-claudeai-optional-one-time)
6. [The full flow: idea to working app](#4-the-full-flow-idea-to-working-app)
7. [What to say to your AI](#what-to-say-to-your-ai)
8. [Troubleshooting](#troubleshooting)
9. [FAQ](#faq)
10. [How it works](#how-it-works)

---



## A 2-minute example: building a notes app

Say you want to build a simple notes and to-do app.

**Monday, in ChatGPT**, you brainstorm:

> **You:** I want to build a notes + to-do app called notes-app. Who is it for?
> **ChatGPT:** … students who want notes and tasks in one place …
> **You:** Save that as a decision: it's for students. And save as rejected: real-time collaboration, too complex for v1.
> **ChatGPT:** ✓ Saved to notes-app.

**Wednesday, in claude.ai**, you keep going:

> **You:** What do we have on notes-app?
> **Claude:** You decided it's for students, and you ruled out real-time collaboration…
> **You:** Let's decide the features… *(more research)* … ok, **ctx spec**.
> **Claude:** ✓ Spec saved.

**Friday, on your computer**, you build it:

```
ctx build notes-app
cd notes-app
claude
```

> **You:** Build this from .ctx/SPEC.md.

Claude Code already knows the app is for students and that collaboration was ruled out. It won't suggest it again, and you never re-explained anything.

That's the whole product. The rest of this page is setup.

---



## 1. Install

One command, nothing to install first. You need [git](https://git-scm.com/downloads)
for syncing later, but not for this.

**macOS and Linux**

```sh
curl -fsSL https://recuros.vercel.app/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm https://recuros.vercel.app/install.ps1 | iex
```

That downloads a single binary for your machine and puts it on your PATH. No
Docker, no Node, no Python, no admin rights.

<details>
<summary>Or build it yourself (needs <a href="https://rustup.rs">Rust</a>)</summary>

```sh
cargo install --git https://github.com/Percobain/RecurOS ctx-cli --locked
```

</details>

Check it worked:

```sh
ctx --version
```

That's it: one program called `ctx`. No Docker, no database, no account.

Every binary is built by GitHub Actions from the tag and published with a
`SHA256SUMS` file, on the [releases page](https://github.com/Percobain/RecurOS/releases).

---



## 2. Use it with Claude Code (local only)

Go into any project folder and run:

```sh
cd my-project
ctx init
```

This connects the project to RecurOS and sets up Claude Code (and Cursor, Codex or Gemini CLI if you have them). **Restart Claude Code** in that folder, and it's ready.

### Already have a project with a history?

An existing codebase already holds most of its own context; it's just spread
across the README, the docs and people's heads. One more command gets it out:

```sh
ctx onboard
```

It prints a prompt. Paste that into Claude Code (or Cursor, or Codex) in that
same folder, and the agent reads your repo and records what it finds: why this
database, what must never break, what was tried and abandoned. Then `ctx pack`
shows you exactly what every AI will be told from now on, and you can delete
anything you disagree with.

From now on, just talk to Claude Code:

> Save that as a decision: we use SQLite, not Postgres, because there's no server to run.

> What did we decide about the database?

> Give me a handoff doc for a new teammate.

> Show me a map of this project.

Claude Code runs the right `ctx` commands itself; you don't need to learn them. (If you're curious: `ctx commands` lists them all.)

**This works on its own with no cloud and no internet.** Steps 3 and 4 only matter if you want ChatGPT and claude.ai to share the same memory.

---



## Self-host a local node relay

Use this path when ChatGPT or claude.ai should reach context that stays on
your computer. The relay gives the connector a stable public URL; your local
`ctx` node makes an authenticated outbound connection to it. Claims,
documents and packs are never stored by the relay.

You need a public HTTPS address for the relay. Run it yourself with Docker:

```sh
cd relay
docker compose run --rm --entrypoint ctx relay relay init --data /data
docker compose up -d
```

This defaults to a small SQLite file under `relay/data`. To store only the
relay's identity/device metadata in MongoDB instead, set `MONGODB_URL` (and
optionally `MONGODB_DATABASE=recuros_relay`) before both `relay init` and
`docker compose up`. Context claims, documents and indexes remain local; full
MongoDB setup notes are in [relay/README.md](relay/README.md).

The first command prints a one-time bootstrap code and connector secret.
Put TLS in front of port 8788, then pair and start the device containing your
context:

```sh
ctx node login --relay https://ctx.example.com --code <bootstrap-code>
ctx node start
```

Configure your chat connector with URL `https://ctx.example.com/mcp` and an
`Authorization: Bearer <connector-secret>` header. A path secret remains
available for older clients, but headers keep the secret out of URLs and
their logs.

The local node must be running for remote requests to succeed. Full setup,
device revocation and backup notes are in [relay/README.md](relay/README.md).

---

## 3. Connect ChatGPT and claude.ai through GitHub (legacy, optional)

ChatGPT and claude.ai can't reach your computer, so they need a small "mailbox" in the cloud. You'll set up two free things, both owned by you:

- a **private GitHub repo**, where your memory is stored and synced;
- a **Cloudflare Worker**, the mailbox that ChatGPT and Claude talk to.

It takes about 15 minutes. You'll need a free [GitHub](https://github.com) account, a free [Cloudflare](https://dash.cloudflare.com/sign-up) account, and [Node.js](https://nodejs.org) 18 or newer.

### Step 3.1: store your memory in a private GitHub repo

1. On GitHub, create a **new, empty, private** repository called `ctx-store`. Don't add a README.
2. Connect your memory to it (replace `YOU` with your GitHub username).
  macOS / Linux:
   Windows (PowerShell):
   (If `~/ctx` doesn't exist yet, run `ctx status` once; it creates it.)

`ctx sync` should end with `synced with https://github.com/YOU/ctx-store.git`.

### Step 3.2: create a GitHub token for the mailbox

The mailbox needs permission to read and write **only** that one repo.

1. Open [github.com/settings/personal-access-tokens/new](https://github.com/settings/personal-access-tokens/new) (a **fine-grained** token).
2. **Token name:** `recuros-worker` (any name works).
3. **Expiration:** 1 year is a good choice.
4. **Repository access:** *Only select repositories*, then pick `ctx-store`.
5. **Permissions**, then *Repository permissions*, then **Contents: Read and write**. GitHub adds *Metadata: Read-only* automatically; that's expected.
6. Click **Generate token** and **copy** it (it starts with `github_pat_`). Keep this page open.



### Step 3.3: put the mailbox on Cloudflare

In a terminal, from the `RecurOS` folder you cloned:

```sh
cd worker
npm install
npx wrangler login
```

A browser window opens. Log in to Cloudflare and click **Allow**.

**Deploy the mailbox** (replace `YOU` with your GitHub username):

```sh
npx wrangler deploy --var GITHUB_REPO:YOU/ctx-store
```

It prints an address like `https://recuros.yourname.workers.dev`. Note it down.

**Give the mailbox your GitHub token.** Run this, and when it asks *"Enter a secret value"*, paste the token from step 3.2 and press Enter:

```sh
npx wrangler secret put GITHUB_TOKEN
```

> ⚠️ Type `GITHUB_TOKEN` exactly as shown: that's the *name*. Paste the token only when it asks for the value. (Putting the token where the name goes is the most common setup mistake.)
>
> On Windows you can skip the paste entirely: copy the token, then run
> `Get-Clipboard | npx wrangler secret put GITHUB_TOKEN`

**Make a private password for the mailbox's address.** This prints a random code:

```sh
node -e "console.log(require('crypto').randomBytes(24).toString('hex'))"
```

Copy it, then run this and paste it when asked:

```sh
npx wrangler secret put CTX_SECRET
```

Secrets take effect immediately; no need to deploy again.

Your **connector URL** is the address from the deploy step, plus `/mcp/`, plus the code:

```
https://recuros.yourname.workers.dev/mcp/<the code>
```

> 🔑 **This URL works like a password.** Anyone with it can read and write your memory. Keep it somewhere safe and don't share screenshots of it.



### Step 3.4: add it to [claude.ai](http://claude.ai)

1. In claude.ai, open **Settings → Connectors** and choose **Add custom connector**.
2. **Name:** `RecurOS`. **URL:** your connector URL.
3. Save. Optionally set the tools to **Always allow** so it doesn't ask every time.

To use it, start a new chat, click the **tools icon** in the message box, and make sure **RecurOS** is switched on.

### Step 3.5: add it to ChatGPT

ChatGPT calls this *Developer mode*. It may require a paid ChatGPT plan.

1. Open **Settings → Security** and turn on **Developer mode**. The "unverified connectors" warning is ChatGPT's standard message for any connector it hasn't reviewed; this one is your own.
2. Open **Settings → Apps & Connectors** and choose **Create** (or *Add custom connector*).
3. **Name:** `RecurOS`. **MCP server URL:** your connector URL. **Authentication:** *No authentication* (the password is already in the URL).
4. Save.

To use it, in a chat click **+**, then **More**, then **RecurOS**.

### Step 3.6: check it works

In ChatGPT or claude.ai, with RecurOS on:

> Use RecurOS: which ideas do I have?

It should list your ideas. If it doesn't, see [Troubleshooting](#troubleshooting).

**Cost:** Cloudflare's free plan allows 100,000 requests a day and **never charges you**: if you ever went over, requests would just fail until the next day. The mailbox also limits itself to about 89,000 a day to stay safely under that.

---



## 4. The full flow: idea to working app

Once steps 1 to 3 are done, this is all you ever do.

### Have an idea (in ChatGPT or claude.ai)

Name your idea when you save the first thing:

> Save this as a new idea called **habit-tracker**: an app that helps people keep daily habits.



### Research it (in either chat, switch whenever you like)

> What do we have on habit-tracker?
> Save that as a decision: streaks reset at midnight in the user's time zone.
> Save as rejected: social leaderboards, because they made people quit in our interviews.
> Save the open question: do we need an Apple Watch app?



### Write the plan

When the research feels done:

> **ctx spec**

The AI writes a complete spec (features, screens, data, tech) and saves it to the idea.

### Build it (on your computer)

```sh
ctx build habit-tracker
cd habit-tracker
claude
```

> Build this from .ctx/SPEC.md.

`ctx build` fetches your latest chat research and creates a project folder with:


| File               | What's in it                                                |
| ------------------ | ----------------------------------------------------------- |
| `.ctx/SPEC.md`     | the spec from your chat                                     |
| `AGENTS.md`        | your decisions, rules, and "rejected: do not suggest again" |
| `.ctx/commands.md` | instructions so the coding agent can use RecurOS for you  |


Already created the folder? Give its path: `ctx build habit-tracker D:\repos\my-folder`.

### Keep going

- Change the plan in a chat later and say **ctx spec** again. The next time you open Claude Code, it updates `.ctx/SPEC.md` and re-reads it.
- Decisions you make while coding ("save that…") go into the idea's **code** section. Your chats can read them too: ask "what do we have on habit-tracker/code?".

---



## What to say to your AI

You never need to learn commands. Say these in ChatGPT, claude.ai or Claude Code:


| You say                                 | What happens                                | Where                                               |
| --------------------------------------- | ------------------------------------------- | --------------------------------------------------- |
| "Save this as a new idea called X"      | starts idea X                               | chats, Claude Code                                  |
| "Save that" / "save that as a decision" | remembers it                                | chats, Claude Code                                  |
| "Save as rejected: …, because …"        | remembers what *not* to do, and why         | chats, Claude Code                                  |
| "Save the open question: …"             | remembers it for later                      | chats, Claude Code                                  |
| "What do we have on X?"                 | reads back everything about idea X          | chats, Claude Code                                  |
| "ctx spec"                              | writes and saves the full plan              | chats                                               |
| "We changed our mind about …"           | replaces the old decision (history is kept) | Claude Code (in a chat, just save the new decision) |
| "Give me a handoff doc"                 | a document a teammate can start from        | Claude Code                                         |
| "Show me a map of the project"          | a diagram of everything decided             | Claude Code                                         |
| "Delete the X idea"                     | removes it everywhere (it asks first)       | Claude Code                                         |


"Chats" means ChatGPT and claude.ai with the RecurOS connector turned on. Claude Code can do everything, because it can run `ctx` itself.

**The AI only saves when you ask.** It never fills your memory with chatter.

### Housekeeping

```sh
ctx list                      # every idea, how big it is, which one this repo uses
ctx rename old-name new-name  # keeps every claim, document and reason
ctx delete <idea> --cloud     # gone from here, GitHub, ChatGPT and claude.ai
```

`ctx delete` never shreds anything: it hides it everywhere and keeps it in the
log, so `ctx log --all` can still show you what used to be there.

### Terminal commands (if you like them)

You only really need three:

```sh
ctx build <idea>     # turn an idea into a project folder for your coding agent
ctx init             # connect an existing project folder
ctx sync             # sync with your chats and your other computers
```

Everything else: `ctx commands` lists every command by what you want to do. `ctx doctor` checks your setup and tells you how to fix anything wrong.

---



## Troubleshooting

**Start here:** `ctx doctor` checks everything and gives a fix for each problem.


| Problem                                             | Fix                                                                                                                                                            |
| --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ChatGPT or Claude saved into the wrong idea         | Name the idea when you save: "save this **to habit-tracker**". To move a note, ask your coding agent, or delete it with `ctx delete c:xxxx` and save it again. |
| The chat printed the spec instead of saving it      | Say: "save that spec to RecurOS with the doc field". Or copy the spec block and run `ctx spec save --paste --to <idea>/research`.                            |
| The connector says "GITHUB_TOKEN secret is not set" | The token was saved under the wrong name. Run `npx wrangler secret put GITHUB_TOKEN` in the `worker` folder and paste the token when asked.                    |
| The connector says "Bad credentials"                | The GitHub token expired or was deleted. Make a new one (step 3.2) and run the command above again.                                                            |
| ChatGPT behaves as if it has old instructions       | ChatGPT caches the tool list. Open *Settings → Apps & Connectors → RecurOS* and refresh it, or remove and re-add the connector.                              |
| Claude Code doesn't seem to know the project        | Run `ctx init` in the folder and restart Claude Code.                                                                                                          |
| `ctx build` says there's no spec                    | Say **ctx spec** in your chat first, then run `ctx build` again.                                                                                               |
| Something I deleted still shows in a chat           | Run `ctx sync` (deletes sync automatically when you're online).                                                                                                |


---



## FAQ

**Is my data sent anywhere?**
Only to places you set up: your own private GitHub repo and your own Cloudflare Worker. There's no RecurOS company server and no tracking.

**Does it use AI to decide what to remember?**
No. It saves exactly what you ask it to, word for word. It never reads your chats on its own.

**Does it cost anything?**
No. Everything runs on your computer, GitHub's free tier and Cloudflare's free tier.

**Do I need the cloud part?**
Only to share memory with ChatGPT and claude.ai. With just Claude Code (or Cursor, Codex, Gemini CLI), step 2 is enough.

**Can I use it on two computers?**
Yes. Install `ctx` on both and point both at the same `ctx-store` repo (step 3.1). They sync without conflicts.

**What if I delete something by mistake?**
Deleted things disappear from your AIs but stay in the history, so nothing is truly lost.

**Which AI tools does it work with?**
ChatGPT and claude.ai (through the connector); Claude Code, Codex, Cursor and Gemini CLI (through `ctx init`); anything else by copy and paste (`ctx pack --clip`). A browser extension for other chat sites is in `[extension/](extension/README.md)`.

---



## How it works

For the curious (and for developers):

```
 ChatGPT / claude.ai ──► your relay ──► your online local ctx node
                                                 │
                                  Claude Code, Codex, Cursor ◄─┘ (AGENTS.md + tools)
```

- Every note is a small **claim** (a decision, fact, rule, rejected idea, open question or belief), stored locally in plain text files that are only ever added to, never edited.
- Each idea has a **research** section (everything you explored) and a **code** section (only the conclusions a builder needs), so your coding agent gets a short, focused briefing instead of every brainstorm.
- Briefings are **size-limited**: the coding agent gets the most useful ~700 tokens, chats get a fuller version, and handoff docs get everything.

Technical details:

- `[docs/protocol.md](docs/protocol.md)`: every file format, rule and design decision, and why.
- `[docs/canonical.md](docs/canonical.md)`: how notes are fingerprinted so they match across machines.
- `[relay/README.md](relay/README.md)`: deploy the self-hosted relay.
- `[worker/README.md](worker/README.md)`: the legacy GitHub-backed Worker path.



### For developers

```sh
cargo test --workspace            # Rust (the ctx program)
cd worker && npm ci && npm test   # the Cloudflare Worker
```


| Folder                                                                     | What                                           |
| -------------------------------------------------------------------------- | ---------------------------------------------- |
| `crates/ctx-cli`                                                           | the `ctx` program                              |
| `crates/ctx-app`                                                           | the operations every surface shares            |
| `crates/ctx-core`, `ctx-store-sqlite`, `ctx-git`, `ctx-branch`, `ctx-pack` | storage, sync, branches, the briefing compiler |
| `crates/ctx-mcp`, `ctx-daemon`, `ctx-wire`, `ctx-relay`                    | the AI tool connections and local-node relay   |
| `worker/`                                                                  | the Cloudflare Worker (ChatGPT / claude.ai)    |
| `extension/`                                                               | the browser extension (coming soon)            |


---



## License

[Apache-2.0](LICENSE).
