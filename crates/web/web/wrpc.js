// wRPC client/server helpers for the browser, built on the
// `@bytecodealliance/wrpc` package (vendored under `./vendor/wrpc` by
// `npm run build:wrpc`; see package.json).
//
// The web UI invokes UTXO methods over wRPC in two configurations, differing
// only in the byte transport:
//
//   - A UTXO served by a `starstream-run` CLI (`--serve <ADDR>`) is invoked
//     over a WebSocket (`webSocketTransport`): the CLI runs the guest and owns
//     the UTXO, alongside the rendered WIT served over plain HTTP at `GET /`.
//   - A UTXO instantiated in the browser is invoked over a duplex stream pair
//     (`streamTransport`) handed to the contract's Web Worker (which runs the
//     guest), so the page is the wRPC client and the Worker the wRPC server.
//
// Either way the invocation itself — the framing, the component-model value
// codec, the multiplexed sub-streams — is handled by the package; this module
// adapts the transports, parses the CLI-served WIT into the type trees the
// codec walks, and bridges between the package's value representation (`jco`
// conventions) and the JSON shape the page's widgets speak.

import { invoke, accept, Chan } from "./vendor/wrpc/index.js";

// ----- WIT type parser ------------------------------------------------------
//
// The CLI renders types with `wasm_wave`'s `DisplayType`, which spells every
// type out structurally and inline (records as `record { a: u8 }`, variants as
// `variant { off, on(u8) }`, …), so the served WIT text is self-contained. This
// tokenizer + recursive-descent parser turns it back into the package's `Type`
// tree (the in-browser path gets the same tree as JSON from the runtime, see
// `type_to_json` in `crates/web/src/lib.rs`).

const TYPE_TOKEN = /\s*(->|[A-Za-z][A-Za-z0-9-]*|[{}()<>,:;_/.@])/y;

function tokenize(src) {
  const tokens = [];
  let pos = 0;
  while (pos < src.length) {
    TYPE_TOKEN.lastIndex = pos;
    const m = TYPE_TOKEN.exec(src);
    if (!m) {
      // Skip any unrecognised character (whitespace runs are consumed by the
      // pattern's leading `\s*`, so this only trips on genuine junk).
      if (/\s/.test(src[pos])) {
        pos += 1;
        continue;
      }
      throw new Error(`unexpected character \`${src[pos]}\` in WIT`);
    }
    tokens.push(m[1]);
    pos = TYPE_TOKEN.lastIndex;
  }
  return tokens;
}

const PRIMITIVES = new Set([
  "bool", "s8", "u8", "s16", "u16", "s32", "u32", "s64", "u64",
  "f32", "f64", "char", "string",
]);

// A token cursor shared by the type and document parsers.
class TokenStream {
  constructor(tokens) {
    this.tokens = tokens;
    this.pos = 0;
  }
  peek() {
    return this.tokens[this.pos];
  }
  next() {
    if (this.pos >= this.tokens.length) throw new Error("unexpected end of WIT");
    return this.tokens[this.pos++];
  }
  expect(tok) {
    const got = this.next();
    if (got !== tok) throw new Error(`expected \`${tok}\`, got \`${got}\``);
    return got;
  }
  eof() {
    return this.pos >= this.tokens.length;
  }
}

// Parse one type from the stream into a package `Type` tree:
//   { kind: "u64" } | { kind: "list", elem } | { kind: "option", some }
//   | { kind: "result", ok, err } | { kind: "tuple", elems: [] }
//   | { kind: "record", fields: [{name, ty}] }
//   | { kind: "variant", cases: [{name, ty|null}] }
//   | { kind: "enum", names: [] } | { kind: "flags", names: [] }
//   | { kind: "own"|"borrow", name }   (e.g. `utxo` / `borrow<utxo>`)
function parseType(ts) {
  const tok = ts.next();
  if (PRIMITIVES.has(tok)) return { kind: tok };
  switch (tok) {
    case "list":
      ts.expect("<");
      const elem = parseType(ts);
      ts.expect(">");
      return { kind: "list", elem };
    case "option":
      ts.expect("<");
      const some = parseType(ts);
      ts.expect(">");
      return { kind: "option", some };
    case "result": {
      if (ts.peek() !== "<") return { kind: "result", ok: null, err: null };
      ts.expect("<");
      const ok = ts.peek() === "_" ? (ts.next(), null) : parseType(ts);
      let err = null;
      if (ts.peek() === ",") {
        ts.next();
        err = parseType(ts);
      }
      ts.expect(">");
      return { kind: "result", ok, err };
    }
    case "tuple": {
      ts.expect("<");
      const elems = [];
      if (ts.peek() !== ">") {
        elems.push(parseType(ts));
        while (ts.peek() === ",") {
          ts.next();
          elems.push(parseType(ts));
        }
      }
      ts.expect(">");
      return { kind: "tuple", elems };
    }
    case "record": {
      ts.expect("{");
      const fields = [];
      if (ts.peek() !== "}") {
        do {
          const name = ts.next();
          ts.expect(":");
          fields.push({ name, ty: parseType(ts) });
        } while (ts.peek() === "," && ts.next());
      }
      ts.expect("}");
      return { kind: "record", fields };
    }
    case "variant": {
      ts.expect("{");
      const cases = [];
      if (ts.peek() !== "}") {
        do {
          const name = ts.next();
          let ty = null;
          if (ts.peek() === "(") {
            ts.next();
            ty = parseType(ts);
            ts.expect(")");
          }
          cases.push({ name, ty });
        } while (ts.peek() === "," && ts.next());
      }
      ts.expect("}");
      return { kind: "variant", cases };
    }
    case "enum": {
      ts.expect("{");
      const names = [];
      if (ts.peek() !== "}") {
        do {
          names.push(ts.next());
        } while (ts.peek() === "," && ts.next());
      }
      ts.expect("}");
      return { kind: "enum", names };
    }
    case "flags": {
      ts.expect("{");
      const names = [];
      if (ts.peek() !== "}") {
        do {
          names.push(ts.next());
        } while (ts.peek() === "," && ts.next());
      }
      ts.expect("}");
      return { kind: "flags", names };
    }
    case "borrow": {
      ts.expect("<");
      const name = ts.next();
      ts.expect(">");
      return { kind: "borrow", name };
    }
    default:
      // A bare identifier left over is an owned resource handle (the CLI spells
      // `own<utxo>` as just `utxo`).
      return { kind: "own", name: tok };
  }
}

// ----- WIT document parser --------------------------------------------------
//
// Parses the regular subset the CLI emits: a `package` declaration and one
// `interface` whose body is a flat list of `name: func(params) -> results;`.
// Returns `{ package, interface, funcs: [{ name, params: [{name, ty}],
// results: [ty] }] }`.

export function parseWit(text) {
  const ts = new TokenStream(tokenize(text));

  let pkg = null;
  if (ts.peek() === "package") {
    ts.next();
    const parts = [];
    while (ts.peek() !== ";" && !ts.eof()) parts.push(ts.next());
    ts.expect(";");
    pkg = parts.join("");
  }

  ts.expect("interface");
  const iface = ts.next();
  ts.expect("{");

  const funcs = [];
  while (ts.peek() !== "}" && !ts.eof()) {
    const name = ts.next();
    ts.expect(":");
    ts.expect("func");
    ts.expect("(");
    const params = [];
    if (ts.peek() !== ")") {
      do {
        const pname = ts.next();
        ts.expect(":");
        params.push({ name: pname, ty: parseType(ts) });
      } while (ts.peek() === "," && ts.next());
    }
    ts.expect(")");

    const results = [];
    if (ts.peek() === "->") {
      ts.next();
      if (ts.peek() === "(") {
        ts.next();
        if (ts.peek() !== ")") {
          do {
            results.push(parseType(ts));
          } while (ts.peek() === "," && ts.next());
        }
        ts.expect(")");
      } else {
        results.push(parseType(ts));
      }
    }
    ts.expect(";");
    funcs.push({ name, params, results });
  }
  ts.expect("}");

  return { package: pkg, interface: iface, funcs };
}

// ----- value <-> JSON -------------------------------------------------------
//
// The package decodes values per the component-model JS (`jco`) conventions
// (`bigint` for 64-bit ints, `{ tag, val }` for variants/results, `undefined`
// for `option` none, …). The page's widgets and the runtime's `val_to_json`
// (`crates/web/src/lib.rs`) instead speak a plain-JSON shape. `valueToJson`
// lowers a decoded value into that shape; arguments need no conversion the
// other way, since the package's encoder already accepts the page's JSON
// (numbers for ints, `null` for `option` none, arrays for `flags`).

// A JS number large enough to lose precision is returned as a string instead,
// so 64-bit values round-trip losslessly through `JSON.stringify`.
function bigintToJson(v) {
  return v >= -9007199254740991n && v <= 9007199254740991n ? Number(v) : v.toString();
}

export function valueToJson(ty, v) {
  switch (ty.kind) {
    case "u64":
    case "s64":
      return bigintToJson(v);
    case "list":
      // `list<u8>` decodes to a `Uint8Array`; render it as a number array.
      return ty.elem.kind === "u8" ? Array.from(v) : v.map((x) => valueToJson(ty.elem, x));
    case "tuple":
      return ty.elems.map((et, i) => valueToJson(et, v[i]));
    case "record": {
      const out = {};
      for (const { name, ty: fty } of ty.fields) out[name] = valueToJson(fty, v[name]);
      return out;
    }
    case "option":
      return v === undefined || v === null ? null : valueToJson(ty.some, v);
    case "result":
      return v.tag === "ok"
        ? { ok: ty.ok ? valueToJson(ty.ok, v.val) : null }
        : { err: ty.err ? valueToJson(ty.err, v.val) : null };
    case "variant": {
      const c = ty.cases.find((c) => c.name === v.tag);
      return c && c.ty ? { [v.tag]: valueToJson(c.ty, v.val) } : v.tag;
    }
    case "flags":
      return ty.names.filter((name) => v[name]);
    case "own":
    case "borrow":
      return { $resource: true };
    default:
      // bool, u8..u32 / s8..s32 (number), f32/f64 (number), char, string,
      // enum (the case name) all pass through unchanged.
      return v;
  }
}

// ----- transports -----------------------------------------------------------
//
// A wRPC `Transport` is a duplex of `Uint8Array` chunks: `read()` (a chunk, or
// `null` at EOF), `write(bytes)`, and `closeWrite()` (half-close). Each adapter
// below also exposes a `closed` promise that resolves once the peer half-closes
// its write side — `invoke` resolves its synchronous results without waiting
// for that EOF (a result-less call has none to read), so callers await `closed`
// to know the server actually ran to completion.

// Adapt a WHATWG duplex (a pair of `{ readable, writable }` streams, as built
// from two `TransformStream`s and shared with a Worker) to a transport.
export function streamTransport({ readable, writable }) {
  const reader = readable.getReader();
  const writer = writable.getWriter();
  let onClosed;
  const closed = new Promise((resolve) => (onClosed = resolve));
  return {
    closed,
    async read() {
      const { value, done } = await reader.read();
      if (done) {
        onClosed();
        return null;
      }
      return value;
    },
    write: (bytes) => writer.write(bytes),
    closeWrite: () => writer.close(),
    async close() {
      onClosed();
      try {
        await reader.cancel();
      } catch {}
      try {
        await writer.abort();
      } catch {}
    },
  };
}

// Adapt a WebSocket carrying the `wrpc-websockets` framing — binary frames are
// data, an empty *text* frame is the write-side EOF sentinel — to a transport.
export function webSocketTransport(url) {
  const ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  const inbound = new Chan();
  let onClosed;
  const closed = new Promise((resolve) => (onClosed = resolve));
  let opened;
  let failed;
  const ready = new Promise((resolve, reject) => {
    opened = resolve;
    failed = reject;
  });
  let settled = false;
  const finish = (err) => {
    if (settled) return;
    settled = true;
    inbound.close(err);
    onClosed();
  };
  ws.onopen = () => opened();
  ws.onmessage = ({ data }) => {
    // An empty text frame is the server's EOF sentinel; binary frames are data.
    if (typeof data === "string") finish();
    else inbound.push(new Uint8Array(data));
  };
  ws.onerror = () => {
    const err = new Error(`WebSocket error connecting to ${url}`);
    failed(err);
    finish(err);
  };
  ws.onclose = () => finish();
  return {
    closed,
    async read() {
      const { value, done } = await inbound.next();
      return done ? null : value;
    },
    async write(bytes) {
      await ready;
      ws.send(bytes);
    },
    async closeWrite() {
      await ready;
      ws.send(""); // empty text frame: the wRPC write-side EOF sentinel
    },
    close() {
      try {
        ws.close();
      } catch {}
    },
  };
}

// ----- invocation helpers ---------------------------------------------------

// Invoke `func` on `instance` over `transport`, encoding `args` (a JSON array)
// against `paramTypes` and lowering the decoded results to the page's JSON
// shape. Waits for the server's EOF so a result-less call still blocks until
// the guest has run, then tears the transport down. Returns a JSON string.
export async function callOverTransport(transport, instance, func, paramTypes, resultTypes, args) {
  try {
    const { results, done } = await invoke(transport, instance, func, paramTypes, args, resultTypes);
    await done;
    await transport.closed;
    return JSON.stringify(resultTypes.map((ty, i) => valueToJson(ty, results[i])));
  } finally {
    transport.close?.();
  }
}

// Serve a single invocation on `transport`: accept it, decode the parameters,
// hand them to `bridge` as a JSON array, and send back the JSON results it
// returns. The write side is always closed, so a `bridge` that throws still
// lets the client's read reach EOF (and the caller report the failure).
export async function serveInvocation(transport, paramTypes, resultTypes, bridge) {
  try {
    const inv = await accept(transport);
    const { params, done } = await inv.receiveParams(paramTypes);
    await done;
    const jsonArgs = paramTypes.map((ty, i) => valueToJson(ty, params[i]));
    const results = await bridge(jsonArgs);
    await inv.sendResults(resultTypes, results);
  } finally {
    try {
      await transport.closeWrite();
    } catch {}
  }
}

// ----- remote contract ------------------------------------------------------

// Normalise a user-entered endpoint into the HTTP base (for the WIT fetch) and
// the WebSocket base (for invocations) the CLI serves on one address.
function resolveEndpoints(input) {
  let raw = input.trim();
  if (!/^[a-z]+:\/\//i.test(raw)) raw = `ws://${raw}`;
  const url = new URL(raw);
  const wsScheme = url.protocol === "https:" || url.protocol === "wss:" ? "wss:" : "ws:";
  const httpScheme = wsScheme === "wss:" ? "https:" : "http:";
  return {
    http: `${httpScheme}//${url.host}/`,
    ws: `${wsScheme}//${url.host}/`,
  };
}

// A client for a single CLI-served contract, exposing the same async surface
// the web UI's local `ContractProxy` does (`describe` / `call` /
// `implementedMethods`) so the invocation panel can drive it uniformly.
export class RemoteContract {
  constructor(endpoint) {
    this.endpoints = resolveEndpoints(endpoint);
    this.wit = null;
    this.parsed = null;
    // Map of `"<instance> <func>"` -> { params: [ty], results: [ty] }.
    this.signatures = new Map();
  }

  // Fetch and parse the served WIT. Returns the raw WIT text.
  async connect() {
    const res = await fetch(this.endpoints.http, { headers: { accept: "text/plain" } });
    if (!res.ok) throw new Error(`HTTP ${res.status} fetching the WIT`);
    this.wit = await res.text();
    this.parsed = parseWit(this.wit);
    for (const func of this.parsed.funcs) {
      this.signatures.set(`${this.instanceName} ${func.name}`, {
        params: func.params.map((p) => p.ty),
        results: func.results,
      });
    }
    return this.wit;
  }

  // The fully-qualified wRPC instance the CLI serves the methods under. UTXO
  // methods live in the `starstream:utxo` package by convention.
  get instanceName() {
    return `starstream:utxo/${this.parsed.interface}`;
  }

  // Same JSON shape as the local `Contract::describe`, so the page renders the
  // served methods with its existing widgets. There are no constructors or
  // storage: the CLI already minted and owns the single served UTXO.
  describe() {
    const methods = this.parsed.funcs.map((func) => ({
      export: func.name,
      label: func.name,
      params: func.params.map((p) => ({ name: p.name, kind: kindOf(p.ty), ty: p.ty })),
      results: func.results,
    }));
    return JSON.stringify({
      instances: [
        {
          name: this.instanceName,
          resource: "utxo",
          constructors: [],
          methods,
          storage: null,
        },
      ],
    });
  }

  // Every served method is, by definition, callable.
  implementedMethods() {
    return JSON.stringify(this.parsed.funcs.map((func) => func.name));
  }

  // Invoke `func` on `instance` over a fresh WebSocket. `argsJson` is the JSON
  // array the panel builds; a leading `{ $handle }` (the injected `self`) is
  // dropped, since the server supplies the receiver. Returns a JSON array of
  // the decoded results.
  async call(instance, func, argsJson) {
    const sig = this.signatures.get(`${instance} ${func}`);
    if (!sig) throw new Error(`unknown method \`${func}\``);
    let args = JSON.parse(argsJson);
    if (args.length && args[0] && typeof args[0] === "object" && "$handle" in args[0]) {
      args = args.slice(1);
    }
    if (args.length !== sig.params.length) {
      throw new Error(`expected ${sig.params.length} argument(s), got ${args.length}`);
    }
    return callOverTransport(
      webSocketTransport(this.endpoints.ws),
      instance,
      func,
      sig.params,
      sig.results,
      args,
    );
  }
}

// The widget tag the UI uses: a scalar type name, or `"json"` for anything that
// needs raw-JSON entry. Mirrors `kind_str` in `crates/web/src/lib.rs`.
function kindOf(ty) {
  return PRIMITIVES.has(ty.kind) ? ty.kind : "json";
}
