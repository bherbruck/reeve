# reeve

reeve (server) compiles desired state; reeve-agent (per box) converges on it.

A Margo-inspired fleet desired-state manager: a layered deployment
tree compiled into per-device render bundles (State-Manifest poll +
content-addressed OCI pull), converged by a pull-based agent.
See CLAUDE.md for the laws and layout; spec/reeve/ for the full spec
and decisions.

## Architecture

Full architecture — server, agent, and everything on the one socket:

![reeve full architecture](docs/media/reeve_full_architecture.svg)

Deployment topologies — single box, hub/spoke federation, air-gapped:

![reeve deployment topologies](docs/media/reeve_deployment_topologies.svg)

For reference, the upstream Margo specification's system design that
reeve implements against (pinned in `spec/margo/`):

![Margo spec system design](docs/media/margo_spec_system_design.svg)

## Setup
    git clone https://github.com/margo/specification spec   # pin PR2 tag
    # sandbox/reference implementation alongside:
    # git clone <margo sandbox repo> reference
    cargo build --workspace
