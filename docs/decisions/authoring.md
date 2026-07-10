# reeve decisions — Tree Authoring (D14)

Part of docs/decisions/; start at [00-INDEX.md](00-INDEX.md).

## D14. Tree authoring is an API — automation-friendly by design
- The revision store's single writer is reeve-server's API. That API
  MUST be first-class for automation: token-authed, idempotent
  "put this layer's content" semantics (same content => no new
  revision, per the D3 no-change-no-commit rule), so IaC workflows
  (operators keeping tree content in their own git repo, reviewing
  via PRs, applying from CI — e.g. `reeve-tree apply ./layers`) are
  a supported front door, not a UI scrape. Git may exist UPSTREAM of
  reeve as a human review ritual; it never exists inside it.

