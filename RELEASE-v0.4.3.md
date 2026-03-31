## v0.4.3

### Features

#### Multi-agent team orchestration
New bootstrap, broadcast, and awareness primitives for coordinating agent teams. Parent agents can now spin up a team of child agents, broadcast instructions, and maintain awareness of each child's state.

#### Simplified skill/plugin system
Consolidated skill and plugin discovery and loading, reducing boilerplate and fixing several orchestration edge cases around skill activation ordering.

#### Registry auth plumbing
Added authentication support for container registries, enabling agents running in private environments to pull images from authenticated sources.

#### Improved child agent context
- Anti-loop parent escalation: child agents now detect circular delegation and escalate back to the parent instead of looping
- BuildKit forwarding: child agents inherit the parent's BuildKit configuration
- Skill preamble injection for child agents

### Fixes

- **Skill preamble leak:** `read_skill` guidance no longer leaks into always-only skill preambles
- **Telegram channel_notify lost:** `channel_notify` was silently consumed as a status-board edit in Telegram instead of being delivered as a message
- **Multi-byte panic in redact_secrets:** Fixed panic when secret value boundaries land on multi-byte UTF-8 codepoints
- **Channel mode parent notification:** Channel mode now correctly notifies the parent agent when a child agent run fails
- **Missing identity seed in channel mode:** Child agents started via channel mode were missing identity files (Agents.md, User.md)
- **Preamble image delivery guidance:** Corrected preamble instructions for delivering images through channel adapters

### Docs

- Updated ARCHITECTURE.md and CLAUDE.md to match current codebase state

### Upgrading

```bash
helm upgrade that-agent oci://ghcr.io/that-labs/helm/that-agent \
  -n <namespace> \
  --reuse-values \
  --set agent.image.tag=v0.4.3
```
