# Tools

27 tools. Generated from `src/tools/mod.rs` and the `Params` structs, and
checked by `bench/verify_tools_doc.py` — if this file and the source disagree, the source wins.

## Choosing one

The tools are deliberately narrow, so picking the right one is most of using them well.

| you have | you want | tool |
|---|---|---|
| words that appear in the note | the notes containing them | `search_text` |
| an idea, not the wording | notes that mean the same thing | `search_semantic` |
| a structural pattern | every literal match | `search_regex` |
| a tag | notes labelled with it | `search_tags` |
| a YAML property | notes whose frontmatter matches | `search_frontmatter` |
| a note | others about the same subject | `note_related` |
| a note | what links to and from it | `note_links` |
| a note path | its text | `note_read` |
| a note path | its tags, headings and links only | `note_metadata` |
| nothing yet | to see how the vault is organised | `vault_list` |

### Reading a semantic result

`search_semantic` and `note_related` return **attribution and evidence as separate fields**, and conflating them is the mistake the design exists to prevent:

- **`match_type`** — *why* the note ranked. `"chunk"` means one passage matched. `"summary"` means the note's overall gist matched.
- **`best_chunk`** — the note's closest passage, with its `heading_path`. Always present when the note has chunks.

When `match_type` is `"summary"`, `best_chunk` is **not** the reason the note was found. Quote it as context, never as the cause.

## Profiles

`OBSIDIAN_TOOLS` accepts a profile name, a comma-separated allow-list, or a `!`-prefixed deny-list.

| profile | tools | includes |
|---|---|---|
| `full` | 27 | everything (default) |
| `core` | 18 | read + write, no semantic search |
| `read` | 17 | **strictly read-only** |
| `minimal` | 6 | the smallest useful set |

No tool mixes reading and writing, which is what makes `read` trustworthy: the filter matches on names, so a tool multiplexing both behind an `action` parameter could not be excluded without losing its read half. `read` is asserted write-free by `read_profile_admits_nothing_that_can_write`.

## Search — finding notes you cannot name

### `search_semantic`

Find notes by meaning rather than wording. Use this when you have a question or an idea and do not know which words the note actually uses; use search_text instead when you know the terms that appear in it. Returns `{ results: [...] }`, ranked most similar first, each hit carrying a snippet and its provenance: `match_type` says WHY the note ranked — "chunk" (one specific passage matched), "summary" (the note's overall gist matched), or "whole_note". `best_chunk` is always present and carries the closest passage with its `heading_path`, but when `match_type` is "summary" that passage did NOT cause the ranking, so do not cite it as the reason the note was found. Requires semantic search to be enabled; while the index is still building this returns an explicit "warming" error with progress rather than degrading silently.

*Profiles: `read`*

| param | type | required | description |
|---|---|---|---|
| `query` | `String` | yes | Natural-language query for semantic search. Does not require exact keyword matches — conceptually similar notes are returned. |
| `top_k` | `Option<usize>` | no | Number of results to return (default: 10). |
| `include_content` | `Option<bool>` | no | If true, include the full note content in each result. Default: false. |
| `lexical_prefetch` | `Option<bool>` | no | When true, first retrieves top candidates via BM25 lexical search, then re-ranks by combining lexical and semantic scores. Produces higher-quality results than either approach alone. Requires both Tantivy and embeddings to be enabled. Default: false. |
| `alpha` | `Option<f32>` | no | Blending weight for hybrid re-ranking: `alpha * BM25 + (1-alpha) * semantic`. Only used when `lexical_prefetch` is true. Lower values favor semantic similarity. Overrides the `OBSIDIAN_HYBRID_ALPHA` env var for this query. Range: 0.0–1.0, default: 0.25. |

### `search_text`

Find notes containing particular words, ranked by BM25. Use when you know terms that literally appear in the note; use search_semantic when you only know the idea. Stems words, so "program" matches "programming", and can tolerate typos when fuzzy matching is enabled. Returns ranked note paths with a relevance score and the matching context snippet.

*Profiles: `core`, `read`, `minimal`*

| param | type | required | description |
|---|---|---|---|
| `query` | `String` | yes | Natural-language search query. Supports stemming (e.g. "program" matches "programming"). Results are ranked by BM25 relevance. |
| `context_length` | `Option<usize>` | no | Characters of context around each match (default: 100). |
| `max_results` | `Option<usize>` | no | Maximum number of file results to return (default: 20). |
| `fuzzy` | `Option<bool>` | no | Enable fuzzy matching with edit distance 1 (tolerates typos). Default: false. |
| `fields` | `Option<Vec<SearchField>>` | no | Restrict search to specific note fields. Default: all fields. Allowed values: `title`, `headings`, `tags`, `body`, `frontmatter`. |

### `search_regex`

Find notes whose text matches a regular expression. Use for structural patterns that word search cannot express — dates, identifiers, TODO markers, code shapes. Prefer search_text for ordinary words: regex does no stemming and no relevance ranking. Returns matching note paths with context snippets.

*Profiles: `core`, `read`*

| param | type | required | description |
|---|---|---|---|
| `pattern` | `String` | yes | Regular expression pattern to search for. |
| `context_length` | `Option<usize>` | no | Characters of context around each match (default: 100). |
| `max_results` | `Option<usize>` | no | Maximum number of file results to return (default: 20). |

### `search_tags`

Find notes carrying a tag, matching both inline #tags and frontmatter tags. Use when filtering by an explicit label the user applied, rather than by what a note says. `include_nested` (default true) also matches children, so "inbox" matches "inbox/read". Returns the matching note paths.

*Profiles: `core`, `read`*

| param | type | required | description |
|---|---|---|---|
| `tag` | `String` | yes | Tag to search for, without the `#` prefix. |
| `include_nested` | `Option<bool>` | no | Also match nested tags, so `inbox` matches `inbox/read`. Default: true. |

### `search_frontmatter`

Find notes by the value of a frontmatter field. Use for structured properties kept in YAML — status, type, author, date — rather than prose; use search_tags for tags specifically. `operator` is "eq" (default), "contains" (substring for strings, membership for arrays), or "exists" (value ignored). Returns the matching note paths.

*Profiles: `core`, `read`*

| param | type | required | description |
|---|---|---|---|
| `field` | `String` | yes | Frontmatter field name to query. |
| `operator` | `Option<FrontmatterOperator>` | no | Comparison operator. Default: `eq`. |
| `value` | `Option<serde_json::Value>` | no | — |

## Relate — starting from a note you have

### `note_related`

Find notes about the same subject as a note you already have, seeded from that note's own embedding — no query string needed. Use when you have a note and want its neighbours; use search_semantic when you have a question instead of a note. Each result carries `linked`: false means no wikilink joins them yet, which is usually the interesting case — a connection the vault has not recorded. Also returns the note's existing outgoing links and backlinks for comparison. Results carry the same `match_type` / `best_chunk` provenance as search_semantic, with the same caveat about summary matches. Requires semantic search to be enabled.

*Profiles: `read`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Vault-relative path of the note to find relatives of. |
| `top_k` | `Option<usize>` | no | Maximum semantically related notes to return (default: 10, max: 50). |
| `include_passages` | `Option<bool>` | no | Include the matched passage and heading path for each result (default: true). Set false for a compact list of paths and scores. |

### `note_links`

Get both link directions for one note in a single call. Use to see where a note sits in the graph as actually recorded; use note_related for connections by meaning that the graph does not yet capture. Returns `{ path, backlinks, outgoing }` — backlinks being the notes that link TO it with the specific wikilinks involved, outgoing being the links FROM it, each with its resolution status (`resolved_path` is null when the link is broken).

*Profiles: `read`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root. |

### `vault_broken_links`

List wikilinks that point at nothing — vault-wide, or within one note if `path` is given. Use to find typos and links left dangling by a rename. Returns each broken link with its source note, the raw link text, and the unresolved target.

*Profiles: `read`*

| param | type | required | description |
|---|---|---|---|
| `path` | `Option<String>` | no | Restrict the scan to a single note. Omit to scan the entire vault. |

### `vault_orphans`

List notes disconnected from the resolvable link graph. Use to find notes nothing points to. Returns each note with a `status` distinguishing "no_links" (nothing in either direction) from "broken_outgoing_only" (it links out, but every one of those links is broken), plus the broken targets involved.

*Profiles: `read`*

No parameters.

## Read

### `note_read`

Read one note's full content as raw markdown, frontmatter included. Use when you know the path and need the text. Use note_read_many for several notes at once, or note_metadata when you only need its tags, headings and links rather than its body.

*Profiles: `core`, `read`, `minimal`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root (e.g. "folder/note.md"). |

### `note_read_many`

Read several notes in one bounded call. Use instead of repeated note_read calls. Provide exactly one of `paths` or `dir`; directory reads are non-recursive unless asked otherwise. Inspects at most 100 files and returns at most 262144 combined content bytes — anything left out is listed in `skipped` with a reason, so check that field rather than assuming you received everything. Fall back to note_read for a single deliberately oversized note.

*Profiles: `core`, `read`*

| param | type | required | description |
|---|---|---|---|
| `paths` | `Option<Vec<String>>` | no | Explicit note paths, in the order they should be returned. Mutually exclusive with `dir`. |
| `dir` | `Option<String>` | no | Directory path relative to vault root. Use an empty string for vault root. Mutually exclusive with `paths`. |
| `recursive` | `Option<bool>` | no | Include note files in nested directories. Defaults to false and is only valid with `dir`. |
| `glob` | `Option<String>` | no | Glob pattern applied to vault-relative paths. Only valid with `dir`. |
| `max_files` | `Option<usize>` | no | Maximum candidate files inspected. Defaults to 20 and is capped at 100. |
| `max_bytes` | `Option<usize>` | no | Maximum combined UTF-8 content bytes returned. Defaults to 65536 and is capped at 262144. |

### `note_metadata`

Get one note's metadata without its body. Use to judge a note cheaply before deciding whether to read it, or to count its backlinks. Returns title, tags, frontmatter, headings, outgoing links, block references, backlinks count and file stats. Use note_read for the text itself.

*Profiles: `core`, `read`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root. |

### `note_frontmatter`

Read a note's frontmatter as JSON, or null if it has none. Read-only — use note_frontmatter_edit to change a field.

*Profiles: `core`, `read`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root. |

### `note_patch_targets`

List the addressable targets in a note: headings with their Markdown level markers ("## Log"), block references, and frontmatter field names. Call this before note_patch to learn exactly which `target` values that note will accept, rather than guessing. Returns `{ headings, block_refs, frontmatter_fields }`.

*Profiles: `core`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root. |

## Write — these modify the vault

### `note_create`

Create a new note, with optional content and YAML frontmatter. Parent directories are created automatically. Fails if the note already exists — use note_write to replace one that does.

*Profiles: `core`, `minimal`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path for the new note, relative to vault root. Parent dirs are created automatically. |
| `content` | `Option<String>` | no | Initial body content. Defaults to empty. |
| `frontmatter` | `Option<serde_json::Value>` | no | — |

### `note_write`

Replace a note's entire content. The note must already exist; use note_create for a new one. This discards whatever is currently there — prefer note_insert to add to a note, or note_patch to change one section of it.

*Profiles: `core`, `minimal`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root. |
| `content` | `String` | yes | New content that replaces the entire note. |

### `note_insert`

Add content to an existing note without replacing what is there. `position` "end" (default) appends; "beginning" inserts after the frontmatter, or at the very start if the note has none. Use note_patch instead when you need to land the content inside a specific section.

*Profiles: `core`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root. |
| `content` | `String` | yes | Content to insert. |
| `position` | `Option<String>` | no | Where to insert: `"end"` (default) appends after existing content; `"beginning"` inserts after frontmatter (or at the very start if no frontmatter). |

### `note_patch`

Modify one section of a note, addressed by heading, block reference, or frontmatter field, with `operation` append, prepend or replace. Call note_patch_targets first to learn the valid `target` values for that note; heading targets accept either bare text ("Log") or the marker-prefixed form ("## Log").

*Profiles: `core`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root. |
| `operation` | `PatchOperation` | yes | Patch operation: `append`, `prepend`, or `replace`. |
| `target_type` | `PatchTargetType` | yes | Target type: `heading`, `block`, or `frontmatter`. |
| `target` | `String` | yes | Target identifier — heading text, block ID, or frontmatter field name. For headings, bare text such as `"Log"` is canonical; ATX marker-prefixed targets such as `"## Log"` are also accepted. |
| `content` | `String` | yes | Content to insert or replace with. |

### `note_frontmatter_edit`

Set or remove a single frontmatter field. `action` is "set" (upsert, requires `value`) or "remove". Pass arrays and objects as real JSON, not as encoded strings. Reading frontmatter is a separate tool, note_frontmatter.

*Profiles: `core`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root. |
| `action` | `String` | yes | Edit to apply: `"set"` (upsert a field) or `"remove"` (delete a field). |
| `key` | `String` | yes | Frontmatter key to set or remove. |
| `value` | `Option<serde_json::Value>` | no | — |

### `note_move`

Move or rename a note. Destination parent directories are created automatically. Wikilinks in other notes are NOT rewritten, so a rename can leave links pointing at the old name — run vault_broken_links afterwards to find any this breaks.

*Profiles: `core`*

| param | type | required | description |
|---|---|---|---|
| `from` | `String` | yes | Current path of the note, relative to vault root. |
| `to` | `String` | yes | Destination path, relative to vault root. |

### `note_delete`

Delete a note from the vault. Requires `confirm: true`, so that a call made by mistake fails instead of destroying a note. This is irreversible and there is no undo.

*Profiles: `core`*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Path to the note, relative to vault root. |
| `confirm` | `bool` | yes | Must be `true` to confirm deletion — a safety check to prevent accidental data loss. |

## Periodic notes

### `periodic_get`

Read the periodic note for a date — daily, weekly, monthly, quarterly or yearly. `date` defaults to today. Read-only: if the note does not exist this returns an error rather than creating it. Use periodic_create to make one.

*Profiles: `read`*

| param | type | required | description |
|---|---|---|---|
| `period` | `NotePeriod` | yes | Period type: daily, weekly, monthly, quarterly, yearly. |
| `date` | `Option<String>` | no | ISO date (YYYY-MM-DD). Defaults to today. |

### `periodic_list`

List recent periodic notes of one period, newest first. Use to find which dates actually have notes before reading them. `limit` defaults to 10. Returns a path and date for each.

*Profiles: `read`*

| param | type | required | description |
|---|---|---|---|
| `period` | `NotePeriod` | yes | Period type: daily, weekly, monthly, quarterly, yearly. |
| `limit` | `Option<usize>` | no | Maximum number of notes to return (default: 10). |

### `periodic_create`

Create the periodic note for a date, expanded from its configured template or from `content` if you supply it. `date` defaults to today. This writes to the vault.

*Profiles: `full` only*

| param | type | required | description |
|---|---|---|---|
| `period` | `NotePeriod` | yes | Period type: daily, weekly, monthly, quarterly, yearly. |
| `date` | `Option<String>` | no | ISO date (YYYY-MM-DD). Defaults to today. |
| `content` | `Option<String>` | no | Custom content; overrides template expansion. |

## Vault

### `vault_list`

List files and directories. Use to explore how the vault is organised when you do not yet know what exists; use the search tools once you know what you are looking for. Supports recursive listing, glob filtering, and a tree view. Returns an array of paths, or objects with title, tags, size and timestamps when `include_metadata` is true; `format: "tree"` returns a formatted string instead.

*Profiles: `core`, `read`, `minimal`*

| param | type | required | description |
|---|---|---|---|
| `path` | `Option<String>` | no | Directory path relative to vault root. Omit or leave empty for vault root. |
| `recursive` | `Option<bool>` | no | List files recursively through subdirectories. Defaults to false. Only used in list mode. |
| `glob` | `Option<String>` | no | Glob pattern to filter results (e.g., `"*.md"`, `"journal/**"`). Only used in list mode. |
| `format` | `Option<String>` | no | Output format: `"list"` (default) returns a JSON array; `"tree"` returns a tree-formatted string. |
| `max_depth` | `Option<usize>` | no | Maximum depth to display. In list mode, limits path component count. In tree mode, limits nesting depth. |
| `include_metadata` | `Option<bool>` | no | Include indexed note metadata in list mode. Defaults to false and is invalid in tree mode. |

### `vault_info`

Aggregate statistics for the whole vault: total notes, files, tags, links and size in bytes. Use to gauge scale before a broad operation, or to confirm which vault you are connected to.

*Profiles: `core`, `read`, `minimal`*

No parameters.

## Utility

### `open_in_obsidian`

Open a note in the Obsidian desktop app via the obsidian:// URI scheme. This acts on the user's machine rather than reading the vault, and requires Obsidian to be installed.

*Profiles: `full` only*

| param | type | required | description |
|---|---|---|---|
| `path` | `String` | yes | Note path relative to vault root. |
| `new_leaf` | `bool` | yes | Open in a new split pane (requires Obsidian Advanced URI plugin). |
