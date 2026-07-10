# reeve-spec

Our own spec, not Margo's. Divergences from `spec/margo/`,
extensions Margo doesn't cover, and decisions made where Margo is
silent (overlay tree, offline behavior, storage, crash-only posture,
providers) get written down here — not scattered in code comments.

See CLAUDE.md, "Spec fidelity" section, for the OURS ENTIRELY vs.
WIRE-EXACT vs. PATTERN-FAITHFUL split this doc set exists to record.

## The spec

One document: **[SPEC.md](SPEC.md)**.

Read Section 3 (Extension Framework & Conformance) first — it is the
load-bearing clause: every extension is additive, and vanilla Margo
tooling MUST interoperate with reeve unmodified, with all extensions
compiled out or disabled. Section 3.5 is the extension index
(REV-001..REV-008 → sections); Section 3.7 is the complete audit of
every touch on a Margo-defined surface.

Extensions keep their REV identifiers (`REV-001` .. `REV-008`)
because the protocol strings (`rev-001/1`, ...) are wire-visible in
capability advertisement; cite them as "SPEC.md Section N (REV-00X)".

## Conventions

RFC 2119 / RFC 8174 requirement language (MUST/SHOULD/MAY in all
capitals). "Law N" citations refer to CLAUDE.md, The Five Laws.
