// SPDX-License-Identifier: Apache-2.0
//
// Thin wrapper around the ai.pipestream.markup.v1 gRPC contract.
//
// The protos are loaded dynamically from ../../../proto (the single source of
// truth in this repository) — no generated code is checked in.

import { fileURLToPath } from "node:url";
import path from "node:path";
import grpc from "@grpc/grpc-js";
import protoLoader from "@grpc/proto-loader";

const PROTO_ROOT = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  "..", "..", "..", "proto",
);

const packageDefinition = protoLoader.loadSync(
  path.join(PROTO_ROOT, "ai", "pipestream", "markup", "v1", "markup_service.proto"),
  {
    includeDirs: [PROTO_ROOT],
    keepCase: false,
    longs: Number,
    enums: String,
    defaults: true,
    oneofs: true,
  },
);

const { ai } = grpc.loadPackageDefinition(packageDefinition);

/** Upload chunk size. Any value gives the same events; this one is quick. */
export const CHUNK_BYTES = 64 * 1024;

/** A connected grpc-markup client. */
export class MarkupClient {
  /** @param {string} address host:port of the grpc-markup server. */
  constructor(address = process.env.MARKUP_ADDR ?? "127.0.0.1:50065") {
    this.stub = new ai.pipestream.markup.v1.MarkupParseService(
      address,
      grpc.credentials.createInsecure(),
    );
  }

  /**
   * Open a ParseMarkup call and send the options frame.
   *
   * The caller then writes `{ chunk }` frames as the document becomes
   * available and calls `.end()`. This is the shape to use when the document
   * is itself arriving from somewhere, since it never holds the whole thing:
   * `server.js` pipes an HTTP upload straight through it.
   *
   * Note the ordering: options go out before anything reads the response. The
   * contract requires the first request message to carry `options`, and a
   * caller that waits for the response before writing waits forever.
   *
   * @param {object} options a ParseOptions message.
   * @returns {object} the duplex call.
   */
  openParse(options) {
    const call = this.stub.parseMarkup();
    call.write({ options });
    return call;
  }

  /**
   * Stream a whole in-memory document and yield each event as it arrives.
   *
   * @param {Buffer} bytes the document.
   * @param {object} options a ParseOptions message.
   * @returns {AsyncGenerator<object>} ParseMarkupResponse messages.
   */
  async *parse(bytes, options) {
    const call = this.openParse(options);

    for (let at = 0; at < bytes.length; at += CHUNK_BYTES) {
      call.write({ chunk: bytes.subarray(at, at + CHUNK_BYTES) });
    }
    call.end();

    // grpc-js hands events to callbacks; this turns the callback stream into
    // an async iterator without buffering the whole document's worth.
    const queue = [];
    let waiting = null;
    let done = false;
    let failure = null;

    const wake = () => {
      if (waiting) {
        const resolve = waiting;
        waiting = null;
        resolve();
      }
    };
    call.on("data", (event) => { queue.push(event); wake(); });
    call.on("end", () => { done = true; wake(); });
    call.on("error", (err) => { failure = err; done = true; wake(); });

    for (;;) {
      while (queue.length > 0) yield queue.shift();
      if (done) break;
      await new Promise((resolve) => { waiting = resolve; });
    }
    if (failure) throw failure;
  }

  /** The server's identity, limits and format list, for the page header. */
  serviceInfo() {
    return new Promise((resolve, reject) => {
      this.stub.getServiceInfo({}, (err, response) => {
        if (err) reject(err); else resolve(response);
      });
    });
  }

  close() {
    grpc.closeClient(this.stub);
  }
}
