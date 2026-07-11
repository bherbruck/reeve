# reeve spec — Operator Fleet Model (REV-010)

Part of the reeve specification; start at [00-INDEX.md](00-INDEX.md).

How operators actually organize and manage a fleet. This is the
**operator-facing** model. The storage engine underneath is unchanged
(content-addressed revisions + overlay-layer merge, docs/decisions/
delivery.md D13, tree-render.md D3/D11) — this section renames the
tiers, adds the missing management write-paths, and mandates that the
UI present INTENT, not the storage plumbing.

## 11.1 The hierarchy

Devices are organized in a fixed, linear config hierarchy. Each level
is an independent grouping a device is assigned to; config merges from
the top down, **deepest wins** (tree-render.md D3), always explainable.

```
All devices        (base — the standard config every device gets)
└─ Fleet           (a logical group of devices)
   └─ Site         (a physical location; a GATEWAY server lives here)
      └─ Device-type   (hardware class, optional per device)
         └─ Device     (this one box)
```

- Org/tenancy and Region are NOT modeled in v1 (single-system). Both
  are additive later without a rebuild — new tiers slot into the merge
  chain by numeric prefix (D12).
- Layer path taxonomy (engine sees opaque `NN-<label>` names, only the
  numeric prefix orders the merge — D12):
  `layers/00-all/`, `layers/10-fleet.<name>/`, `layers/20-site.<name>/`,
  `layers/30-type.<name>/`, `layers/40-device.<id>/`.
- A device's chain = `00-all` + its assigned fleet/site/type layers (if
  set) + its own device layer. Assignment comes from the device row
  (§11.3), never from tree content.

## 11.2 Tags

Devices carry free-form key/value **tags**. Tags are for ad-hoc
grouping, filtering, and rollout cohort selection (09-rollouts) ONLY.
Tags MUST NOT select or inject configuration — that is the layer
chain's job (D12 labels rule, restated). A device can carry any number
of tags; they are orthogonal to the hierarchy.

## 11.3 Device management (the write paths — NEW)

Every attribute a device row holds MUST be manageable from the API and
the web UI (not just at enrollment):

- **Assignment:** `fleet`, `site`, `type` — moving a device between
  groups. Changing any of these re-renders the device (its layer chain
  changed) so its config updates.
- **Tags:** add/remove free-form key/value tags.
- **Display name:** a human rename, distinct from the immutable
  `device_id`.
- **Pin:** a boolean hold. A pinned device keeps its current desired
  config and is excluded from new deploys/rollouts until unpinned
  (it still counts as converged in gate math, 09-rollouts D12).
- **Decommission:** revoke the device credential and tombstone the
  device (its desired state stops being served). Idempotent.

Wire:
- `PATCH /api/devices/{id}` — partial update of
  `{displayName?, fleet?, site?, type?, pinned?, tags?}` (null clears an
  assignment). Human auth, operator+. Re-renders on assignment change.
- `POST /api/devices/{id}/decommission` — revoke + tombstone.
- Enrollment MAY pre-assign: a join token carries optional
  `{fleet?, site?, type?, tags?}` applied to devices that enroll with
  it (agent.md D4), so a box lands in the right group at first contact.

## 11.4 Deploy = intent, not layer editing

Operators deploy a **stack** (a workload/app) to a **scope**, never by
editing a numbered layer directly:

- `POST /api/deploy` `{ stack, scope }` where `scope` is one of
  `{kind: "fleet"|"site"|"type", name}` (authors the app into that
  hierarchy layer), `{kind: "all"}` (the base layer), or
  `{kind: "devices", ids: [...]}` (authors into each device layer).
- Undeploy is the same call removing the app from the scope.
- Under the hood this is a normal authoring commit (D14); the operator
  sees "deploy nginx to Site plant-a", not "PUT layers/20-site.plant-a".
- Tag-scoped and multi-device targeting for STAGED delivery is a
  rollout (§11.5), not a direct deploy.

## 11.5 History and rollouts — no revision-picking in the UI

- The revision store still records every change attributably (D13), but
  the UI presents it as **History** (who changed what, when) with
  **Undo** (which internally authors a new revision restoring prior
  content). The words "tree", "revision", "layer", "blame", and numeric
  layer paths MUST NOT appear in operator-facing copy.
- A **rollout** is "roll out the current desired config to a scope in
  waves, with health gates and auto-pause" (09-rollouts). It targets a
  scope (§11.4) + optional tag cohort, NOT a revision id chosen by the
  operator. Rollback is "undo" / redeploy the previous config as a new
  rollout — never surfaced as "select revision N".

## 11.6 Server tier declaration

A server optionally declares its level in the topology:

- `REEVE_TIER` = `root` (default — the cloud/hub) | `site` (an on-prem
  site gateway). A `site` tier MUST also set `REEVE_SITE=<name>` (the
  site it serves) and `REEVE_UPSTREAM` (its parent), per federation
  (06-federation §8.1). A gateway belongs to exactly one Site and can
  never serve a level above it.
- `REEVE_TIER` is a convenience/clarity declaration; the operative
  federation behavior remains driven by `REEVE_UPSTREAM` presence (D9).
  A `site` tier without `REEVE_UPSTREAM` is a config error.

## 11.7 UI mandate

The fleet UI MUST let an operator, without touching a terminal or the
API directly: browse the hierarchy (drill Fleet → Site → Type →
Device), see and edit a device (rename, move between groups, tag, pin,
decommission), deploy/undeploy a stack to a scope, run and watch a
rollout, and read History with Undo. "Actually manageable from the
web" is the acceptance bar (docs/build-charter.md UI track intent).
