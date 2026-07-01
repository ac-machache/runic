# runic-serve HTTP API

An HTTP + SSE surface over a runic `Agent`. A **thread** is a session; a **run**
is one agent invocation, streamed as Server-Sent Events.

The machine-readable spec is served at **`GET /openapi.json`** (OpenAPI 3.1).
Build with `--features docs-ui` to also mount Swagger UI at **`/docs`**.

## Authentication / tenant model

There is no built-in auth — put this behind a gateway. Every request reads an
optional **`X-Runic-Tenant`** header; when absent it falls back to `default`.
All data (threads, artifacts, asks) is scoped by tenant, so tenant A can never
see or address tenant B's resources.

## Error shape

Every failure returns the same body:

```json
{ "error": "bad_request", "message": "..." }
```

| `error`          | status | when                                            |
|------------------|--------|-------------------------------------------------|
| `bad_request`    | 400    | malformed body, bad cursor, invalid artifact ref |
| `not_found`      | 404    | unknown thread or run                           |
| `store`          | 500    | session-store backend fault                     |
| `internal`       | 500    | unexpected server-side invariant                |
| `upstream`       | 502    | transcription backend failed                    |
| `not_configured` | 501    | feature (e.g. transcription) not wired          |
| `agent`          | 500    | defined but not surfaced over HTTP — agent/run failures arrive as a `run_error` **SSE event** on a 200 stream, not an error body |

## Thread lifecycle

```bash
# create (optionally with a client-chosen id and label)
curl -sX POST localhost:8080/threads \
  -H 'x-runic-tenant: alice' -H 'content-type: application/json' \
  -d '{"thread_id":"t1","label":"my chat"}'

curl -s localhost:8080/threads/t1 -H 'x-runic-tenant: alice'          # fetch
curl -sX PATCH localhost:8080/threads/t1 -H 'x-runic-tenant: alice' \
  -H 'content-type: application/json' -d '{"label":null}'             # clear label
curl -s 'localhost:8080/threads?limit=50' -H 'x-runic-tenant: alice' # list (paged)
curl -sX DELETE localhost:8080/threads/t1 -H 'x-runic-tenant: alice' # delete + artifacts
```

`GET /threads` pages newest-active-first; when more remain the response carries
`next_cursor`, which you pass back as `?cursor=`.

## Run streaming

`POST /threads/{id}/runs/stream` drives a turn and streams events as
`text/event-stream`. Body is either `{"message":"..."}` or a full
`{"content":[...]}` block array.

```bash
curl -N -X POST localhost:8080/threads/t1/runs/stream \
  -H 'x-runic-tenant: alice' -H 'content-type: application/json' \
  -d '{"message":"hello"}'
```

Event names: `run_start`, `assistant_text_delta`, `assistant_thinking_delta`,
`tool_start`, `tool_finish`, `turn_complete`, `usage`, `ask_required`,
`escalated`, `warning`, `run_error`, `done`. The stream always ends with
`done`; a provider failure emits `run_error` then `done`. Each event's `type`
field equals its SSE `event:` name.

### Human-in-the-loop

When a run calls `ask_user` it emits `ask_required` with an `ask_id` and parks.
Answer it (the run resumes):

```bash
curl -sX POST localhost:8080/threads/t1/asks/<ask_id> \
  -H 'x-runic-tenant: alice' -H 'content-type: application/json' \
  -d '{"answer":"yes"}'    # 202 on success, 400 if no such pending ask
```

## Replay

`GET /threads/{id}/runs/{run_id}/stream` replays a run's persisted events, then
attaches to the live tail if it's still in flight. Send **`Last-Event-ID`** (the
`id:` of the last SSE line you saw) to resume — only events with a greater seq
are replayed. Ends with `done`.

```bash
curl -N localhost:8080/threads/t1/runs/<run_id>/stream \
  -H 'x-runic-tenant: alice' -H 'last-event-id: 12'
```

`GET /threads/{id}/events` returns the same log as a paged JSON snapshot
(`?after_seq=&limit=`) for a non-streaming history load.

## Artifacts

Upload raw bytes; reference the returned id from a run instead of inlining
base64. `Content-Type` sets the media type (default
`application/octet-stream`); `X-Runic-Filename` is optional and echoed back.

```bash
id=$(curl -sX POST localhost:8080/threads/t1/artifacts \
  -H 'x-runic-tenant: alice' -H 'content-type: application/pdf' \
  -H 'x-runic-filename: doc.pdf' --data-binary @doc.pdf | jq -r .id)

curl -sX POST localhost:8080/threads/t1/runs/stream \
  -H 'x-runic-tenant: alice' -H 'content-type: application/json' \
  -d "{\"content\":[{\"type\":\"text\",\"text\":\"summarize\"},
       {\"type\":\"artifact_ref\",\"id\":\"$id\",\"media_type\":\"application/pdf\"}]}"
```

Inline media posted directly to a run is stored and replaced with a reference
before it reaches the event log; a reference to an artifact you don't own is
rejected with 400. Uploads via `/artifacts` cap at 25 MiB.

Run request bodies (`/runs/stream`) are intentionally capped at ~2 MiB so the
streaming turn stays lightweight — anything larger returns 413. Upload big media
to `/artifacts` and reference it by id rather than inlining it in a run.

## Transcription

`POST /transcribe` turns audio into text (a preprocessing step — the audio
never enters a thread). Requires an `audio/*` `Content-Type`; returns
`{ "text": ..., "language": ... }`. 501 if no backend is configured. Max 100 MiB.

```bash
curl -sX POST localhost:8080/transcribe \
  -H 'x-runic-tenant: alice' -H 'content-type: audio/wav' \
  --data-binary @clip.wav
```

## Testing against Postgres

The default test suite uses in-memory stores. To verify against a real
Dockerized Postgres (Postgres session + artifact metadata, local blob bytes):

```bash
bash crates/runic-serve/scripts/test-postgres.sh
```
