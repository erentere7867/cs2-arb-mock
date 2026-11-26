# Workstream B — Network resilience

Owns per-endpoint token buckets, complete request suppression during 429
cooldowns, and exponential retry backoff with jitter (1 s base, 5 min cap)
for 5xx and disconnects. Includes deterministic timing tests.
