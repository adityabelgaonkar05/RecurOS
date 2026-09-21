// Cloudflare Worker implementation of the local-first relay. It holds only
// connector/device metadata and a short-lived tunnel URL in a Durable Object;
// MCP bodies are forwarded directly to the user's local ctx node.

import { blake3 } from "@noble/hashes/blake3.js";
import { DurableObject } from "cloudflare:workers";

const text = new TextEncoder();
const decoder = new TextDecoder();
const registrationPrefix = text.encode("recuros-node-register-v1\0");
const forwardPrefix = text.encode("recuros-node-forward-v1\0");
const registrationTtlSeconds = 120;
const clockSkewSeconds = 60;

export interface Env {
  /** One-time first-device pairing code, set with `wrangler secret put`. */
  BOOTSTRAP_CODE?: string;
  /** MCP connector credential. It is not a device credential. */
  CTX_SECRET?: string;
  LIMITER?: RateLimit;
  RELAY: DurableObjectNamespace<RelayCoordinator>;
}

interface EnrollRequest {
  bootstrap_code?: unknown;
  public_key?: unknown;
}

interface RegisterRequest {
  device_id?: unknown;
  public_url?: unknown;
  expires_at?: unknown;
  signature?: unknown;
}

interface Device {
  publicKey: string;
  revoked: boolean;
  url?: string;
  expiresAt?: number;
}

interface SigningMaterial {
  privateKey: string;
  publicKey: string;
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

function bytes(...parts: Uint8Array[]): Uint8Array {
  const result = new Uint8Array(parts.reduce((size, part) => size + part.length, 0));
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.length;
  }
  return result;
}

function int64(value: number): Uint8Array {
  const result = new Uint8Array(8);
  new DataView(result.buffer).setBigInt64(0, BigInt(value));
  return result;
}

function encode(value: Uint8Array): string {
  let binary = "";
  for (const byte of value) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
}

function decode(value: string): Uint8Array {
  const padded = value.replaceAll("-", "+").replaceAll("_", "/") + "===".slice((value.length + 3) % 4);
  const binary = atob(padded);
  return Uint8Array.from(binary, (char) => char.charCodeAt(0));
}

function now(): number {
  return Math.floor(Date.now() / 1000);
}

function rpcError(payload: unknown, code: number, message: string): unknown {
  if (Array.isArray(payload)) {
    return payload.filter((item) => item && typeof item === "object" && "id" in item).map((item) => rpcError(item, code, message));
  }
  const id = payload && typeof payload === "object" && "id" in payload ? (payload as { id?: unknown }).id ?? null : null;
  return { jsonrpc: "2.0", id, error: { code, message } };
}

function registrationBytes(deviceId: string, publicUrl: string, expiresAt: number): Uint8Array {
  return bytes(registrationPrefix, text.encode(deviceId), new Uint8Array([0]), text.encode(publicUrl), new Uint8Array([0]), int64(expiresAt));
}

function forwardBytes(deviceId: string, timestamp: number, nonce: string, body: Uint8Array): Uint8Array {
  return bytes(forwardPrefix, text.encode(deviceId), new Uint8Array([0]), int64(timestamp), text.encode(nonce), new Uint8Array([0]), blake3(body));
}

function publicNodeUrl(value: unknown): string | null {
  if (typeof value !== "string") return null;
  try {
    const url = new URL(value);
    const host = url.hostname.toLowerCase();
    const isIpv4 = /^\d{1,3}(\.\d{1,3}){3}$/.test(host);
    if (
      url.protocol !== "https:" ||
      url.username ||
      url.password ||
      url.search ||
      url.hash ||
      (url.pathname !== "/" && url.pathname !== "") ||
      host === "localhost" ||
      host.includes(":") ||
      isIpv4
    ) {
      return null;
    }
    url.pathname = "/";
    return url.toString().replace(/\/$/, "");
  } catch {
    return null;
  }
}

function connectorSecret(request: Request, url: URL): string | null {
  const bearer = request.headers.get("authorization")?.match(/^Bearer (.+)$/)?.[1];
  if (bearer) return bearer;
  const parts = url.pathname.split("/");
  return url.pathname.startsWith("/mcp/") && parts.length === 3 ? parts[2] ?? null : null;
}

async function parseJson(request: Request): Promise<unknown | null> {
  try {
    return await request.json();
  } catch {
    return null;
  }
}

/** A single durable, SQLite-backed control plane for one Worker deployment. */
export class RelayCoordinator extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/v1/health" && request.method === "GET") {
      return json({ ok: true, service: "recuros-worker-relay" });
    }
    if (url.pathname === "/v1/node/enroll" && request.method === "POST") return this.enroll(request);
    if (url.pathname === "/v1/node/register" && request.method === "POST") return this.register(request);
    if ((url.pathname === "/mcp" || url.pathname.startsWith("/mcp/")) && request.method === "POST") {
      return this.forward(request, url);
    }
    return new Response("not found", { status: 404 });
  }

  private async signing(): Promise<SigningMaterial> {
    const existing = await this.ctx.storage.get<SigningMaterial>("signing");
    if (existing) return existing;
    const generated = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"])) as CryptoKeyPair;
    const material = {
      privateKey: encode(new Uint8Array((await crypto.subtle.exportKey("pkcs8", generated.privateKey)) as ArrayBuffer)),
      publicKey: encode(new Uint8Array((await crypto.subtle.exportKey("raw", generated.publicKey)) as ArrayBuffer)),
    };
    await this.ctx.storage.put("signing", material);
    return material;
  }

  private async enroll(request: Request): Promise<Response> {
    const body = (await parseJson(request)) as EnrollRequest | null;
    if (!body || typeof body.bootstrap_code !== "string" || typeof body.public_key !== "string") {
      return json({ error: "invalid enrolment" }, 400);
    }
    if (!this.env.BOOTSTRAP_CODE || body.bootstrap_code !== this.env.BOOTSTRAP_CODE) {
      return json({ error: "bootstrap code is invalid or has already been used" }, 401);
    }
    if (await this.ctx.storage.get<boolean>("bootstrap_used")) {
      return json({ error: "bootstrap code is invalid or has already been used" }, 401);
    }
    try {
      const publicKey = decode(body.public_key);
      if (publicKey.length !== 32) throw new Error("length");
      await crypto.subtle.importKey("raw", publicKey, { name: "Ed25519" }, false, ["verify"]);
    } catch {
      return json({ error: "device public key is invalid" }, 400);
    }
    const deviceId = crypto.randomUUID();
    await this.ctx.storage.put(`device:${deviceId}`, { publicKey: body.public_key, revoked: false } satisfies Device);
    await this.ctx.storage.put("bootstrap_used", true);
    const signing = await this.signing();
    return json({ device_id: deviceId, relay_public_key: signing.publicKey });
  }

  private async register(request: Request): Promise<Response> {
    const body = (await parseJson(request)) as RegisterRequest | null;
    if (!body || typeof body.device_id !== "string" || typeof body.expires_at !== "number" || typeof body.signature !== "string") {
      return json({ error: "invalid registration" }, 400);
    }
    const publicUrl = publicNodeUrl(body.public_url);
    if (!publicUrl || !Number.isSafeInteger(body.expires_at) || body.expires_at <= now() || body.expires_at > now() + registrationTtlSeconds) {
      return json({ error: "invalid node URL or expiry" }, 401);
    }
    const device = await this.ctx.storage.get<Device>(`device:${body.device_id}`);
    if (!device || device.revoked) return json({ error: "unknown or revoked device" }, 401);
    try {
      const key = await crypto.subtle.importKey("raw", decode(device.publicKey), { name: "Ed25519" }, false, ["verify"]);
      const valid = await crypto.subtle.verify({ name: "Ed25519" }, key, decode(body.signature), registrationBytes(body.device_id, publicUrl, body.expires_at));
      if (!valid) throw new Error("signature");
    } catch {
      return json({ error: "invalid device registration signature" }, 401);
    }
    const previousId = await this.ctx.storage.get<string>("active_device");
    if (previousId && previousId !== body.device_id) {
      const previous = await this.ctx.storage.get<Device>(`device:${previousId}`);
      if (previous) await this.ctx.storage.put(`device:${previousId}`, { ...previous, url: undefined, expiresAt: undefined });
    }
    await this.ctx.storage.put(`device:${body.device_id}`, { ...device, url: publicUrl, expiresAt: body.expires_at });
    await this.ctx.storage.put("active_device", body.device_id);
    return json({ ok: true });
  }

  private async forward(request: Request, url: URL): Promise<Response> {
    const secret = connectorSecret(request, url);
    const raw = new Uint8Array(await request.arrayBuffer());
    const payload = (() => {
      try {
        return JSON.parse(decoder.decode(raw));
      } catch {
        return null;
      }
    })();
    if (payload === null) return json(rpcError(null, -32700, "Invalid JSON-RPC body."), 400);
    if (!secret || !this.env.CTX_SECRET || secret !== this.env.CTX_SECRET) {
      return json(rpcError(payload, -32001, "Missing or invalid connector credential."), 401);
    }
    const activeId = await this.ctx.storage.get<string>("active_device");
    const device = activeId ? await this.ctx.storage.get<Device>(`device:${activeId}`) : undefined;
    if (!device || device.revoked || !device.url || !device.expiresAt || device.expiresAt < now()) {
      return json(rpcError(payload, -32002, "The RecurOS node for this account is offline. Start `ctx node start` on the paired device."), 503);
    }
    const signing = await this.signing();
    const timestamp = now();
    const nonce = encode(crypto.getRandomValues(new Uint8Array(16)));
    const key = await crypto.subtle.importKey("pkcs8", decode(signing.privateKey), { name: "Ed25519" }, false, ["sign"]);
    const signature = await crypto.subtle.sign({ name: "Ed25519" }, key, forwardBytes(activeId!, timestamp, nonce, raw));
    const target = new URL("v1/execute", `${device.url}/`);
    try {
      const response = await fetch(target, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "x-ctx-device": activeId!,
          "x-ctx-timestamp": String(timestamp),
          "x-ctx-nonce": nonce,
          "x-ctx-signature": encode(new Uint8Array(signature)),
        },
        body: raw,
      });
      if (!response.ok) throw new Error(`node returned ${response.status}`);
      return new Response(response.body, { status: 200, headers: { "Content-Type": "application/json" } });
    } catch {
      return json(rpcError(payload, -32002, "The RecurOS node for this account is offline. Start `ctx node start` on the paired device."), 503);
    }
  }
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if ((url.pathname === "/mcp" || url.pathname.startsWith("/mcp/")) && env.LIMITER) {
      const { success } = await env.LIMITER.limit({ key: "global" });
      if (!success) return json(rpcError(null, -32000, "RecurOS daily budget guard: try again shortly."), 429);
    }
    return env.RELAY.get(env.RELAY.idFromName("owner")).fetch(request);
  },
} satisfies ExportedHandler<Env>;
