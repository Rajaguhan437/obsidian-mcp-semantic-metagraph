# Tool reference

**20 tools.** Parameter names below are the **actual JSON schema names**, read
out of the source rather than written from memory — several differ from what you
would guess (`note_move` takes `from`/`to`, not `from_path`/`to_path`;
`search_metadata` takes `type`, not `search_type`).

Anything marked **required** has no default and the call fails without it.
Defaults in the notes column are what the server applies when the field is
omitted.

## Index

| tool | does | needs semantic |
|---|---|---|
| [`search_semantic`](#search_semantic) | meaning-based retrieval over passages | yes |
| [`search_text`](#search_text) | BM25 full-text with field boosts | no |
| [`search_regex`](#search_regex) | regex across note bodies | no |
| [`search_metadata`](#search_metadata) | query tags and frontmatter | no |
| [`note_related`](#note_related) | notes about the same subject as a given note | yes |
| [`wikilinks`](#wikilinks) | backlinks, outgoing, broken, orphans | no |
| [`note_read`](#note_read) | full markdown of one note | no |
| [`note_read_many`](#note_read_many) | several notes, byte-capped | no |
| [`note_inspect`](#note_inspect) | headings, tags, block refs, backlink count | no |
| [`frontmatter`](#frontmatter) | read or modify frontmatter fields | no |
| [`note_create`](#note_create) | create a note | no |
| [`note_write`](#note_write) | replace a note's whole content | no |
| [`note_insert`](#note_insert) | insert content at a position | no |
| [`note_patch`](#note_patch) | targeted edit at a heading, block or field | no |
| [`note_move`](#note_move) | move or rename | no |
| [`note_delete`](#note_delete) | delete, with an explicit confirm | no |
| [`vault_list`](#vault_list) | list notes and directories | no |
| [`vault_info`](#vault_info) | counts, tags, index status | no |
| [`periodic`](#periodic) | daily/weekly/monthly periodic notes | no |
| [`open_in_obsidian`](#open_in_obsidian) | open a note in the Obsidian app | no |

"Needs semantic" means the tool requires a build with `embeddings` or
`embeddings-api` **and** `OBSIDIAN_EMBEDDINGS=true`. The other 18 work with
neither.

---

## Search

### `search_semantic`

Meaning-based retrieval over chunk and summary vectors.

| param | required | notes |
|---|---|---|
| `query` | **yes** | natural language; no keyword overlap needed |
| `top_k` | no | default 10 |
| `include_content` | no | default false; true returns each note's full body |
| `lexical_prefetch` | no | default false. **Leave it false** — see below |
| `alpha` | no | BM25 weight in the legacy prefetch path only; 0.0–1.0, default 0.25 |

**Results carry retrieval provenance** — `match_type`, `best_chunk`,
`summary_score`. See [Retrieval provenance](../README.md#retrieval-provenance)
for what those mean and when each is present.

`score` is a **ranking score, not a cosine**: a weighted summary match can exceed
1.0.

**On `lexical_prefetch`:** true enables the legacy BM25-gated re-rank, which
scores only the top `DEFAULT_PREFETCH_COUNT = 50` lexical hits. A note BM25 never
surfaces cannot be recovered. On the development corpus that cap cost up to
**0.30 nDCG**. It is retained for compatibility, not because it helps.

While the index is warming this returns an explicit "not ready" error rather than
silently degrading to lexical-only results.

### `search_text`

Tantivy BM25 across title, headings, tags, frontmatter and the **full body**,
with field boosts (title 5.0, tags 4.0, headings 3.0, frontmatter 2.0, body 1.0).
Use it for exact strings, identifiers and terminology.

| param | required | notes |
|---|---|---|
| `query` | **yes** | stemmed — "program" matches "programming" |
| `context_length` | no | default 100, capped at 2000 |
| `max_results` | no | default 20, capped at 200 |
| `fuzzy` | no | default false; edit distance 1 |
| `fields` | no | subset of `title`, `headings`, `tags`, `body`, `frontmatter`; default all |

### `search_regex`

Regex across note bodies. Pattern length and compiled size are bounded.

| param | required | notes |
|---|---|---|
| `pattern` | **yes** | |
| `context_length` | no | default 100 |
| `max_results` | no | default 20 |

### `search_metadata`

Structured queries over tags and frontmatter.

| param | required | notes |
|---|---|---|
| `type` | **yes** | `"tag"` or `"frontmatter"`. **The JSON key is `type`**, not `search_type` |
| `tag` | when `type` is `tag` | a leading `#` is stripped if present |
| `include_nested` | no | default **true** — `inbox` also matches `inbox/read` |
| `field` | when `type` is `frontmatter` | frontmatter key |
| `value` | for `eq` and `contains` | pass arrays/objects as JSON; a JSON-encoded string compares as a literal string |
| `operator` | no | `eq` (default), `contains`, `exists` |

---

## Relate

### `note_related`

What else in the vault is about this note. Seeded from the note's own stored
embedding, so it needs no query string and costs no embedding call.

| param | required | notes |
|---|---|---|
| `path` | **yes** | vault-relative path of the subject note |
| `top_k` | no | default 10, max 50 |
| `include_passages` | no | default true; false omits passage text for a compact list |

**Returns** the semantically nearest notes *and* the note's existing links, so
the two can be compared:

| field | notes |
|---|---|
| `related[]` | nearest notes, each with `score`, `match_type`, `best_chunk`, and **`linked`** |
| `linked.outgoing` / `linked.backlinks` | what the graph already records |
| `unlinked_related` | how many of `related` are not linked either way |

**`linked: false` is the interesting case** — a note clearly about the same
subject that the vault does not connect. Scoring is identical to
`search_semantic`, so `score` is comparable between the two tools.

The seed is the note's **summary** vector (title + every heading + first 400
words), which answers "what is this note about" rather than "what matches one of
its paragraphs". Notes indexed with `OBSIDIAN_SUMMARY_WEIGHT=0` fall back to
their first chunk.

Errors explicitly when the subject note has no embeddings yet, rather than
reporting that nothing is similar.

---

## Graph

### `wikilinks`

| param | required | notes |
|---|---|---|
| `query` | **yes** | `backlinks` \| `outgoing` \| `broken` \| `orphans` |
| `path` | for `backlinks` and `outgoing` | optional for `broken`, unused for `orphans` |

- **backlinks** — sources linking to a note, with each link's raw text and line
- **outgoing** — links from a note, each with `resolved_path` (null if broken),
  `heading`, `block_ref`, `alias`
- **broken** — links whose target does not resolve
- **orphans** — disconnected notes, with `status` separating "no links at all"
  from "only broken links"

Results are **not deterministically ordered** (hash-map iteration). Compare
order-insensitively; do not snapshot-test the sequence.

---

## Read

### `note_read`

| param | required | notes |
|---|---|---|
| `path` | **yes** | vault-relative, e.g. `"folder/note.md"` |

Returns the full markdown, frontmatter included.

### `note_read_many`

Several notes at once, byte-capped so a large selection cannot flood a context
window.

| param | required | notes |
|---|---|---|
| `paths` | either this or `dir` | explicit paths, returned in the given order |
| `dir` | either this or `paths` | vault-relative directory; empty string means vault root |
| `recursive` | no | default false; only valid with `dir` |
| `glob` | no | pattern over vault-relative paths; only valid with `dir` |
| `max_files` | no | default 20, capped |
| `max_bytes` | no | default 65536 combined UTF-8 bytes |

`paths` and `dir` are mutually exclusive.

### `note_inspect`

| param | required | notes |
|---|---|---|
| `path` | **yes** | |
| `view` | no | `"metadata"` (default) or an alternate view |

Headings, tags, block references and backlink count — without the body. Cheaper
than `note_read` when you only need structure.

### `frontmatter`

| param | required | notes |
|---|---|---|
| `action` | **yes** | `"get"`, `"set"`, `"remove"` |
| `path` | **yes** | |
| `key` | for `set` and `remove` | frontmatter key |
| `value` | for `set` | JSON value; arrays and objects pass through directly |

---

## Write

All writes update the vault index, the lexical index and the link graph
incrementally, and queue the note for re-embedding.

### `note_create`

| param | required | notes |
|---|---|---|
| `path` | **yes** | parent directories are created |
| `content` | no | defaults to empty |
| `frontmatter` | no | JSON object, e.g. `{"tags": ["rust"]}` |

### `note_write`

| param | required | notes |
|---|---|---|
| `path` | **yes** | |
| `content` | **yes** | replaces the entire note |

### `note_insert`

| param | required | notes |
|---|---|---|
| `path` | **yes** | |
| `content` | **yes** | |
| `position` | no | `"end"` (default) appends after existing content |

### `note_patch`

Targeted edit without rewriting the note.

| param | required | notes |
|---|---|---|
| `path` | **yes** | |
| `operation` | **yes** | `append`, `prepend`, `replace` |
| `target_type` | **yes** | `heading`, `block`, `frontmatter` |
| `target` | **yes** | heading text, block ID, or frontmatter field |
| `content` | **yes** | |

**Nested headings use `::`** — `"Introduction::Background"` targets the
`Background` heading beneath `Introduction` (`HEADING_DELIMITER`,
`src/vault/patch.rs`).

### `note_move`

| param | required | notes |
|---|---|---|
| `from` | **yes** | **not** `from_path` |
| `to` | **yes** | **not** `to_path` |

### `note_delete`

| param | required | notes |
|---|---|---|
| `path` | **yes** | |
| `confirm` | **yes** | must be `true`; there is no default |

---

## Navigate

### `vault_list`

| param | required | notes |
|---|---|---|
| `path` | no | directory; omit or empty for vault root |
| `recursive` | no | default false |
| `glob` | no | e.g. `"*.md"`, `"journal/**"` |
| `format` | no | `"list"` (default, JSON array) or `"tree"` |
| `max_depth` | no | limits depth in tree mode, path components in list mode |
| `include_metadata` | no | default false; adds indexed note metadata in list mode |

### `vault_info`

**No parameters.** Returns counts, tags, index status, active exclusion patterns,
and the resolved data directory.

### `periodic`

| param | required | notes |
|---|---|---|
| `action` | **yes** | `"get"`, `"create"`, `"list"` |
| `period` | **yes** | `daily`, `weekly`, `monthly`, `quarterly`, `yearly` |
| `date` | no | ISO `YYYY-MM-DD`; defaults to today |
| `content` | no | overrides template expansion; `create` only |
| `limit` | no | default 10; `list` only |

### `open_in_obsidian`

| param | required | notes |
|---|---|---|
| `path` | **yes** | |
| `new_leaf` | no | default false; new split pane, needs the Advanced URI plugin |

Opens via the `obsidian://` URI scheme, so it needs the Obsidian app running.
Every other tool works whether Obsidian is open or not.

---

## Restricting the surface

`OBSIDIAN_TOOLS` accepts a profile, a comma-separated allow-list, or a
`!`-prefixed deny-list.

| profile | tools | for |
|---|---|---|
| `full` | 20 | everything (default) |
| `core` | 15 | read + write, without semantic, graph or periodic |
| `read` | 12 | read-only, including `search_semantic` and `note_related` |
| `minimal` | 6 | read, create, write, list, text search, info |

```bash
OBSIDIAN_TOOLS=read
OBSIDIAN_TOOLS='!note_delete,!note_write,!note_patch'
```

Giving an agent `read` is the safest way to let it explore a vault it must not
modify.

---

*Parameter names and requirements here were extracted from the `Params` structs
in `src/tools/`, not transcribed. If you change a tool's schema, this file is
what goes stale: the profile counts are covered by tests in
`tests/integration_tests.rs`, the parameter names are not.*
