# Operations — Kubernetes Backend

Resource lifecycle, storage, rollouts, cleanup, and capacity management on Kubernetes.

## Resource Labeling

- Use standard label conventions: app name, component, managed-by, part-of
- All resources created by the agent must carry a consistent managed-by label
- Use label selectors for rollout checks, log tailing, and cleanup operations

## Storage

- **PersistentVolumeClaims** for data that must survive pod restarts
- Set StorageClass explicitly or verify the default is correct for the workload
- Size PVCs for expected usage — oversizing wastes, undersizing crashes silently
- Monitor PVC usage — full volumes cause application failures without obvious errors
- Reclaim policy: `Delete` reclaims on PVC deletion, `Retain` keeps data for manual recovery
- Ephemeral data (caches, build artifacts) should use `emptyDir`, not PVCs

## Rollouts

- Always use rolling updates with readiness probes
- Wait for rollout completion before declaring success
- If rollout stalls, investigate (`describe` deployment, check events) — do not force
- Keep previous ReplicaSets available for quick rollback
- A failed rollout should be diagnosed, not retried blindly

## Cleanup

- Delete completed Jobs after collecting output
- Remove failed/evicted pods for managed labels after recovery
- Clean stale ConfigMaps and Secrets from previous deployments
- Watch for PVCs in `Released` state that are no longer needed
- Never let orphaned resources accumulate — verify scope before bulk deletion

## Capacity

- Set resource requests and limits on all workloads
- Requests guarantee minimum allocation; limits cap maximum usage
- Missing requests: unpredictable scheduling. Missing limits: unbounded consumption
- Watch for pending pods — usually means insufficient resources or unschedulable constraints
- Check node conditions and resource pressure before deploying heavy workloads

## Secrets

- Kubernetes Secrets for sensitive data, ConfigMaps for non-sensitive configuration
- Mount as environment variables or volume files depending on the consumer
- Consider external secret operators for production
- Rotate secrets by updating the Secret resource and restarting affected workloads

## Incident Response

1. **Scope** — single pod, deployment, namespace, or cluster-wide?
2. **Events** — check namespace events for scheduling, pull, or policy errors
3. **Logs** — pod logs for application failures
4. **Describe** — resource specs for misconfigurations
5. **Fix or rollback** — apply fix if clear; otherwise rollback to previous revision
6. **Document** — store incident details in memory

Never force-delete pods, skip probes, or scale to zero as a "fix" — find the root cause.
