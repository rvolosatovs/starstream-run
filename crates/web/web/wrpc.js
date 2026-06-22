// A dependency-free wRPC-over-WebSocket client for the browser.
//
// The `starstream-run` CLI's `--serve` mode mints a single UTXO and serves its
// ABI methods over wRPC framed on WebSockets, alongside the rendered WIT served
// over plain HTTP at `GET /`. This module lets the web UI act as that client:
// it fetches and parses the served WIT, then invokes a method by opening a
// WebSocket, encoding the parameters in the wRPC wire format and decoding the
// results back.
//
// Wire protocol (see the wRPC `SPEC.md` and `wrpc-transport`'s framing):
//   - One WebSocket connection per invocation (the framed transport maps a
//     single stream to a single call).
//   - The client writes, as binary frames: a `0x00` protocol byte, the
//     `core:name`-encoded instance and function names, then a single root frame
//     `[path-len=0][data-len][params]` whose data is the concatenated
//     component-model encoding of the parameters (the `self` receiver is
//     injected server-side, so it is omitted here).
//   - The write side is then closed with an empty WebSocket *text* frame, the
//     EOF sentinel `wrpc-websockets` uses.
//   - The server replies with the concatenated encoding of the results in a
//     root frame, then its own empty-text EOF.
//
// Values are encoded/decoded with the same component-model value codec the CLI
// uses (`crates/cli/src/codec.rs`): ints are LEB128 (except `s8`/`u8`/`bool`/
// floats), strings are `core:name`, and so on.

// ----- byte writer / reader -------------------------------------------------

class Writer {
  constructor() {
    this.bytes = [];
  }
  u8(b) {
    this.bytes.push(b & 0xff);
  }
  raw(arr) {
    for (const b of arr) this.bytes.push(b & 0xff);
  }
  // Unsigned LEB128 of a non-negative BigInt.
  lebU(value) {
    let v = BigInt(value);
    if (v < 0n) throw new Error(`expected a non-negative integer, got ${v}`);
    do {
      let byte = Number(v & 0x7fn);
      v >>= 7n;
      if (v !== 0n) byte |= 0x80;
      this.bytes.push(byte);
    } while (v !== 0n);
  }
  // Signed LEB128 of a BigInt.
  lebS(value) {
    let v = BigInt(value);
    for (;;) {
      const byte = Number(v & 0x7fn);
      v >>= 7n; // arithmetic shift (BigInt `>>` floors)
      const signBit = byte & 0x40;
      if ((v === 0n && !signBit) || (v === -1n && signBit)) {
        this.bytes.push(byte);
        return;
      }
      this.bytes.push(byte | 0x80);
    }
  }
  // A `core:name`: a LEB128 length prefix followed by the UTF-8 bytes.
  name(str) {
    const utf8 = new TextEncoder().encode(str);
    this.lebU(BigInt(utf8.length));
    this.raw(utf8);
  }
  finish() {
    return Uint8Array.from(this.bytes);
  }
}

class Reader {
  constructor(bytes) {
    this.bytes = bytes;
    this.pos = 0;
  }
  get done() {
    return this.pos >= this.bytes.length;
  }
  u8() {
    if (this.pos >= this.bytes.length) throw new Error("unexpected end of input");
    return this.bytes[this.pos++];
  }
  take(n) {
    if (this.pos + n > this.bytes.length) throw new Error("unexpected end of input");
    const slice = this.bytes.subarray(this.pos, this.pos + n);
    this.pos += n;
    return slice;
  }
  lebU() {
    let result = 0n;
    let shift = 0n;
    let byte;
    do {
      byte = this.u8();
      result |= BigInt(byte & 0x7f) << shift;
      shift += 7n;
    } while (byte & 0x80);
    return result;
  }
  lebS() {
    let result = 0n;
    let shift = 0n;
    let byte;
    do {
      byte = this.u8();
      result |= BigInt(byte & 0x7f) << shift;
      shift += 7n;
    } while (byte & 0x80);
    if (byte & 0x40) result |= -1n << shift; // sign-extend
    return result;
  }
  name() {
    const len = Number(this.lebU());
    return new TextDecoder().decode(this.take(len));
  }
}

// A JS number large enough to lose precision is returned as a string instead,
// so 64-bit values round-trip losslessly through `JSON.stringify`.
function bigintToJson(v) {
  return v >= -9007199254740991n && v <= 9007199254740991n ? Number(v) : v.toString();
}

// ----- WIT type parser ------------------------------------------------------
//
// The CLI renders types with `wasm_wave`'s `DisplayType`, which spells every
// type out structurally and inline (records as `record { a: u8 }`, variants as
// `variant { off, on(u8) }`, …), so the served WIT text is self-contained. This
// tokenizer + recursive-descent parser turns it back into a type tree the value
// codec walks.

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

// Parse one type from the stream into a tree:
//   { kind: "u64" } | { kind: "list", elem } | { kind: "option", some }
//   | { kind: "result", ok, err } | { kind: "tuple", elems: [] }
//   | { kind: "record", fields: [{name, ty}] }
//   | { kind: "variant", cases: [{name, ty|null}] }
//   | { kind: "enum", names: [] } | { kind: "flags", names: [] }
//   | { kind: "resource", name }   (e.g. `utxo` / `borrow<utxo>`)
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
      return { kind: "resource", name };
    }
    default:
      // A bare identifier left over is an owned resource handle (the CLI spells
      // `own<utxo>` as just `utxo`).
      return { kind: "resource", name: tok };
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

// ----- value codec ----------------------------------------------------------
//
// Encodes/decodes a single component-model value against a parsed type, in the
// wRPC wire format. Mirrors `crates/cli/src/codec.rs`.

function encodeValue(w, ty, v) {
  switch (ty.kind) {
    case "bool":
      w.u8(v ? 1 : 0);
      return;
    case "u8":
      w.u8(Number(v) & 0xff);
      return;
    case "s8":
      w.u8(Number(v) & 0xff);
      return;
    case "u16": case "u32": case "u64":
      w.lebU(BigInt(v));
      return;
    case "s16": case "s32": case "s64":
      w.lebS(BigInt(v));
      return;
    case "f32": {
      const buf = new ArrayBuffer(4);
      new DataView(buf).setFloat32(0, Number(v), true);
      w.raw(new Uint8Array(buf));
      return;
    }
    case "f64": {
      const buf = new ArrayBuffer(8);
      new DataView(buf).setFloat64(0, Number(v), true);
      w.raw(new Uint8Array(buf));
      return;
    }
    case "char": {
      const utf8 = new TextEncoder().encode(String(v));
      w.raw(utf8);
      return;
    }
    case "string":
      w.name(String(v));
      return;
    case "list": {
      if (!Array.isArray(v)) throw new Error("expected an array for list");
      w.lebU(BigInt(v.length));
      for (const x of v) encodeValue(w, ty.elem, x);
      return;
    }
    case "tuple": {
      if (!Array.isArray(v) || v.length !== ty.elems.length) {
        throw new Error(`expected a ${ty.elems.length}-tuple`);
      }
      ty.elems.forEach((et, i) => encodeValue(w, et, v[i]));
      return;
    }
    case "record": {
      if (typeof v !== "object" || v === null) throw new Error("expected an object for record");
      for (const { name, ty: fty } of ty.fields) {
        if (!(name in v)) throw new Error(`missing record field \`${name}\``);
        encodeValue(w, fty, v[name]);
      }
      return;
    }
    case "option": {
      if (v === null || v === undefined) {
        w.u8(0);
      } else {
        w.u8(1);
        encodeValue(w, ty.some, v);
      }
      return;
    }
    case "result": {
      if (typeof v !== "object" || v === null || (!("ok" in v) === !("err" in v))) {
        throw new Error("expected `{ ok }` or `{ err }` for result");
      }
      if ("ok" in v) {
        w.u8(0);
        if (ty.ok) encodeValue(w, ty.ok, v.ok);
      } else {
        w.u8(1);
        if (ty.err) encodeValue(w, ty.err, v.err);
      }
      return;
    }
    case "enum": {
      const i = ty.names.indexOf(String(v));
      if (i < 0) throw new Error(`unknown enum case \`${v}\``);
      w.lebU(BigInt(i));
      return;
    }
    case "variant": {
      // Accepts `"case"` for payload-less cases or `{ "case": value }`.
      let name, payload;
      if (typeof v === "string") {
        name = v;
        payload = undefined;
      } else if (v && typeof v === "object") {
        name = Object.keys(v)[0];
        payload = v[name];
      } else {
        throw new Error("expected a string or single-key object for variant");
      }
      const i = ty.cases.findIndex((c) => c.name === name);
      if (i < 0) throw new Error(`unknown variant case \`${name}\``);
      w.lebU(BigInt(i));
      if (ty.cases[i].ty) encodeValue(w, ty.cases[i].ty, payload);
      return;
    }
    case "flags": {
      const set = new Set(Array.isArray(v) ? v : []);
      const bytes = new Uint8Array(Math.ceil(Math.max(ty.names.length, 1) / 8));
      ty.names.forEach((name, i) => {
        if (set.has(name)) bytes[i >> 3] |= 1 << (i & 7);
      });
      w.raw(bytes);
      return;
    }
    default:
      throw new Error(`encoding \`${ty.kind}\` is not supported`);
  }
}

function decodeValue(r, ty) {
  switch (ty.kind) {
    case "bool":
      return r.u8() !== 0;
    case "u8":
      return r.u8();
    case "s8": {
      const b = r.u8();
      return b >= 0x80 ? b - 0x100 : b;
    }
    case "u16": case "u32": case "u64":
      return bigintToJson(r.lebU());
    case "s16": case "s32": case "s64":
      return bigintToJson(r.lebS());
    case "f32":
      return new DataView(r.take(4).slice().buffer).getFloat32(0, true);
    case "f64":
      return new DataView(r.take(8).slice().buffer).getFloat64(0, true);
    case "char": {
      // Read one UTF-8 scalar value: peek the lead byte to learn its length.
      const lead = r.bytes[r.pos];
      const len = lead < 0x80 ? 1 : lead < 0xe0 ? 2 : lead < 0xf0 ? 3 : 4;
      return new TextDecoder().decode(r.take(len));
    }
    case "string":
      return r.name();
    case "list": {
      const n = Number(r.lebU());
      const out = [];
      for (let i = 0; i < n; i++) out.push(decodeValue(r, ty.elem));
      return out;
    }
    case "tuple":
      return ty.elems.map((et) => decodeValue(r, et));
    case "record": {
      const out = {};
      for (const { name, ty: fty } of ty.fields) out[name] = decodeValue(r, fty);
      return out;
    }
    case "option":
      return r.u8() !== 0 ? decodeValue(r, ty.some) : null;
    case "result": {
      const isErr = r.u8() !== 0;
      if (isErr) return { err: ty.err ? decodeValue(r, ty.err) : null };
      return { ok: ty.ok ? decodeValue(r, ty.ok) : null };
    }
    case "enum": {
      const i = Number(r.lebU());
      return ty.names[i] ?? `enum#${i}`;
    }
    case "variant": {
      const i = Number(r.lebU());
      const c = ty.cases[i];
      if (!c) return `variant#${i}`;
      return c.ty ? { [c.name]: decodeValue(r, c.ty) } : c.name;
    }
    case "flags": {
      const bytes = r.take(Math.ceil(Math.max(ty.names.length, 1) / 8));
      return ty.names.filter((_, i) => bytes[i >> 3] & (1 << (i & 7)));
    }
    case "resource":
      return { $resource: true };
    default:
      throw new Error(`decoding \`${ty.kind}\` is not supported`);
  }
}

// ----- invocation -----------------------------------------------------------

const PROTOCOL = 0x00;

// Build the bytes the client writes on the invocation's root channel: the
// protocol byte, the instance and function names, and the params root frame.
function encodeInvocation(instance, func, paramTypes, args) {
  const params = new Writer();
  paramTypes.forEach((ty, i) => encodeValue(params, ty, args[i]));
  const paramBytes = params.finish();

  const w = new Writer();
  w.u8(PROTOCOL);
  w.name(instance);
  w.name(func);
  w.u8(0); // root frame path length
  w.lebU(BigInt(paramBytes.length)); // root frame data length
  w.raw(paramBytes);
  return w.finish();
}

// Open a fresh WebSocket, send the invocation, and resolve with the raw bytes
// the server sends back on the root channel (its results frame data). Rejects
// on a socket error or if the connection closes before the EOF sentinel.
function invokeRaw(wsUrl, payload) {
  return new Promise((resolve, reject) => {
    let ws;
    try {
      ws = new WebSocket(wsUrl);
    } catch (err) {
      reject(err);
      return;
    }
    ws.binaryType = "arraybuffer";
    const chunks = [];
    let settled = false;

    ws.onopen = () => {
      ws.send(payload);
      ws.send(""); // empty text frame: the wRPC write-side EOF sentinel
    };
    ws.onmessage = ({ data }) => {
      if (typeof data === "string") {
        // The server's empty-text EOF sentinel: the response is complete.
        settled = true;
        let total = 0;
        for (const c of chunks) total += c.length;
        const buf = new Uint8Array(total);
        let off = 0;
        for (const c of chunks) {
          buf.set(c, off);
          off += c.length;
        }
        resolve(buf);
        ws.close();
      } else {
        chunks.push(new Uint8Array(data));
      }
    };
    ws.onerror = () => {
      if (!settled) reject(new Error(`WebSocket error connecting to ${wsUrl}`));
    };
    ws.onclose = () => {
      if (!settled) reject(new Error("WebSocket closed before the EOF sentinel"));
    };
  });
}

// Decode the server's root-frame bytes into the result values. The frame is
// `[path-len=0][data-len][data]`; `data` is the concatenated results.
function decodeResults(bytes, resultTypes) {
  if (bytes.length === 0) return []; // no results frame written (empty results)
  const frame = new Reader(bytes);
  const pathLen = Number(frame.lebU());
  if (pathLen !== 0) throw new Error(`unexpected result frame path length ${pathLen}`);
  const dataLen = Number(frame.lebU());
  const data = new Reader(frame.take(dataLen));
  return resultTypes.map((ty) => decodeValue(data, ty));
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
      params: func.params.map((p) => ({ name: p.name, kind: kindOf(p.ty) })),
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
    const payload = encodeInvocation(instance, func, sig.params, args);
    const bytes = await invokeRaw(this.endpoints.ws, payload);
    return JSON.stringify(decodeResults(bytes, sig.results));
  }
}

// The widget tag the UI uses: a scalar type name, or `"json"` for anything that
// needs raw-JSON entry. Mirrors `kind_str` in `crates/web/src/lib.rs`.
function kindOf(ty) {
  return PRIMITIVES.has(ty.kind) ? ty.kind : "json";
}
