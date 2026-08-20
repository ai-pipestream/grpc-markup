# Node web demo

A live viewer for the grpc-markup server. Stubs are loaded dynamically from
[`../../proto`](../../proto) at run time, so nothing generated is checked in.

```bash
npm install
npm start          # then open http://127.0.0.1:8088
```

The bridge honours `MARKUP_ADDR` (default `127.0.0.1:50065`) and `PORT`
(default `8088`).

### Serving under a base path

Set `UI_BASE` and the whole viewer moves under that prefix, for example behind
a reverse proxy that forwards `/ui/markup/*` unchanged — which is how the
shared demo shell mounts it:

```bash
UI_BASE=/ui/markup npm start   # page at http://127.0.0.1:8088/ui/markup/
```

The bridge strips the prefix before routing, so every endpoint lives at
`$UI_BASE/api/*`, and it injects a `<meta name="ui-base">` tag into the served
page, which the page reads to prefix its own `fetch()` calls. Unset, nothing
changes: the bridge answers at the root exactly as before.

## The web viewer

The viewer exists to make one property visible: **every format lands on the
same block vocabulary**. Pick a sample in each format and the feed below the
upload bar shows the same `TextBlock`, `TableRow`, `ImageBlock` and `Cue`
events whichever source wrote them.

It is a single HTTP request. The browser POSTs the document and reads
Server-Sent Events off the *same* response, which is deliberately the same
shape as the gRPC call underneath it: bytes going one way while events come
back the other. Nothing buffers the document. Each upload slice is written
into the gRPC call as it lands, and each event is flushed to the page as the
Rust server emits it.

Three sources, in the order they win:

- **Paste** markup into the text box.
- **Pick** a local file.
- **Choose a sample** from [`../sample-data`](../sample-data) — one per format
  the server maps, mirroring the golden fixtures the Rust test suite pins.

The format dropdown lists whatever the running server reports in
`GetServiceInfo.formats`, plus *sniff it*, which leaves the format unset and
lets the server's signatures decide. A sample whose extension names a format
pre-selects it. The **also fold to a Document** checkbox sets
`ParseOptions.emit_document`, adding the one `document` event immediately
before the trailer.

The page shows an upload bar with a green marker where the first block
landed, per-label counts in the sidebar, and the trailer's aggregated
warnings in red. The throttle delays the *upload* only; the parser is never
waiting on anything but bytes.

Worth trying:

| Document | What you see |
|---|---|
| `kitchen-sink.vtt` | cues with millisecond timings and voice-span speakers; two malformed cues become one aggregated warning |
| `meeting-notes.boxnote.json` on *sniff it* | the ProseMirror `doc` signature resolves the format; the unknown node type warns and the parse continues |
| `quarterly-review.docling.json` with the fold on | the Docling payload carried over item for item into one `document` event |
| `streaming-markup.tex` | the section ladder, spans in the tabular, and one aggregated warning for the unknown environment |

## Things that bite

**Write the options frame before you read the response.** The contract
requires the first request message to carry `options`. `lib/markup.js` sends
it inside `openParse()` for exactly this reason, so the ordering cannot be
got wrong by a caller.

**Handle the oneof by name, not by guessing.** With `oneofs: true`,
proto-loader sets `message.event` to the name of the active arm. The bridge
forwards `message[message.event]` rather than sniffing which key is
populated, so an arm added to the contract later is passed through to the
page instead of being dropped silently.

**Backpressure is real and worth keeping.** `res.write()` returning false
means the browser is behind. The bridge pauses the gRPC call and resumes on
`drain`, which propagates through gRPC flow control back to the parser.
