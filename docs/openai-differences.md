# Where omni2api differs from OpenAI on purpose

Behaviour that knowingly departs from OpenAI's API semantics, and why.
Anything not listed here aims to match OpenAI.

- **Background responses are stored even with `store: false`.** A
  `background: true` response is kept until it finishes, whatever `store`
  says: polling `GET /v1/responses/{id}` and `cancel` need the object, and
  the seat answers on its own schedule. Delete it when done if it must not
  linger.
- **Vault credential secrets are write-only.** As at OpenAI, no read
  (create, get, list, update) returns a credential's tokens or secrets.
  This server goes further and does not keep them at all, so a stored
  credential documents what was configured but cannot hand a secret to a
  session later.
- **Vector-store search sees a bounded set of files.** Filters on file
  `attributes` are applied by the server before the seat sees anything;
  past `OMNI2API_VECTOR_SEARCH_MAX_BYTES` only the most relevant (then
  newest) matching files are attached (see `server-env.md`).
