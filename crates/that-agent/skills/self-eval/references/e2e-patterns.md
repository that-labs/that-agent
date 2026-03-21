# E2E Backend Testing Patterns

Patterns for building evaluation scenarios that test an agent's ability to coordinate
end-to-end backend testing — service startup, API interaction, log analysis, and
state verification.

## Pattern 1: Service Health Check

Test whether the agent can verify a running service is healthy and report status.

```toml
[[steps]]
type    = "run_command"
command = "docker compose -f /tmp/test-stack/docker-compose.yml up -d"

[[steps]]
type    = "prompt"
session = "main"
content = "Check if the backend services are healthy and report any issues."

[[steps]]
type = "assert"
[[steps.assertions]]
kind    = "tool_call_seen"
tool    = "shell_exec"
min_count = 1

[[steps]]
type    = "run_command"
command = "docker compose -f /tmp/test-stack/docker-compose.yml down"
```

## Pattern 2: API Flow Verification

Test whether the agent can walk through a multi-step API flow and verify correctness.

```toml
# Setup: start service + seed data
[[steps]]
type    = "run_command"
command = "cd /tmp/test-app && ./scripts/seed-test-data.sh"

# Ask agent to verify the flow
[[steps]]
type    = "prompt"
session = "main"
content = """
Run through the user registration flow on our test API:
1. Create a new user
2. Verify the user appears in the database
3. Test the login endpoint with the new credentials
4. Report any failures
"""

# Assert agent actually made API calls
[[steps]]
type = "assert"
[[steps.assertions]]
kind    = "tool_call_seen"
tool    = "shell_exec"
min_count = 3

# Teardown
[[steps]]
type    = "run_command"
command = "cd /tmp/test-app && ./scripts/cleanup-test-data.sh"
```

## Pattern 3: Log Analysis After Failure

Test the agent's ability to diagnose issues from logs.

```toml
# Setup: start service with a known bug
[[steps]]
type    = "run_command"
command = "cd /tmp/test-app && INJECT_BUG=missing_index ./start.sh"

# Trigger the bug
[[steps]]
type    = "run_command"
command = "curl -s http://localhost:3000/api/slow-endpoint > /dev/null"

# Ask agent to investigate
[[steps]]
type    = "prompt"
session = "main"
content = "Users are reporting slow response times. Check the logs and find the root cause."

[rubric]
[[rubric.criteria]]
name        = "root_cause_identification"
description = "Agent identified the missing database index as the root cause from log analysis"
weight = 40

[[rubric.criteria]]
name        = "evidence_based_reasoning"
description = "Agent cited specific log entries or metrics rather than guessing"
weight = 30

[[rubric.criteria]]
name        = "remediation_proposed"
description = "Agent suggested a concrete fix (migration, index creation) not just diagnosis"
weight = 30
```

## Pattern 4: Multi-Service Coordination

Test agent's understanding of service dependencies and inter-service communication.

```toml
[[steps]]
type    = "run_command"
command = "docker compose -f /tmp/microservices/docker-compose.yml up -d"

[[steps]]
type    = "prompt"
session = "main"
content = """
We have three services running: gateway, auth, and orders.
The orders service is returning 500 errors. Trace the issue across services
and identify which service is the actual source of the problem.
"""

[[steps]]
type = "assert"
[[steps.assertions]]
kind    = "tool_call_seen"
tool    = "shell_exec"
min_count = 2

[rubric]
[[rubric.criteria]]
name        = "cross_service_tracing"
description = "Agent examined logs or state from multiple services, not just the one returning errors"
weight = 40

[[rubric.criteria]]
name        = "correct_root_service"
description = "Agent correctly identified the originating service rather than the symptom service"
weight = 35

[[rubric.criteria]]
name        = "fix_proposal"
description = "Agent proposed a targeted fix for the root service"
weight = 25
```

## Pattern 5: Database State Verification

Test agent's ability to verify data consistency after operations.

```toml
[[steps]]
type    = "run_command"
command = "cd /tmp/test-app && ./scripts/run-migration.sh"

[[steps]]
type    = "prompt"
session = "main"
content = """
We just ran a data migration. Verify that:
1. All user records still have valid email addresses
2. No orphaned records exist in the orders table
3. The new column 'status' has been populated for existing rows
Report any inconsistencies.
"""

[rubric]
[[rubric.criteria]]
name        = "systematic_verification"
description = "Agent ran targeted queries for each verification point rather than a single broad check"
weight = 40

[[rubric.criteria]]
name        = "accuracy"
description = "Agent's findings match the actual database state"
weight = 35

[[rubric.criteria]]
name        = "clear_reporting"
description = "Results presented in a structured, actionable format"
weight = 25
```

## Conventions

- Always use `run_command` for setup and teardown — keep the test environment clean
- Sandbox mode (`sandbox = true`) is required when scenarios create/delete infrastructure
- Use `{{agent_name}}` placeholder for agent-specific paths
- Tag E2E scenarios with `["e2e", "<service-domain>"]`
- Keep timeouts realistic for service startup: `timeout_secs = 300` for Docker-based tests
- Assert tool usage first, then use judge rubrics for quality assessment
