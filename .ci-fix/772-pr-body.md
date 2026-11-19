## Oversight audit limits

When an audit connection must be discarded, the probe waits up to the configured database pool wait for a replacement named `oversight audit`. If checkout fails, the parent audit row may be absent. The probe logs the complete parent summary at error level and returns the checkout failure. Confirmed child audit rows remain committed.

## Exceptions

apps/base/grid/src/oversight.rs: get_detached: A replacement checkout gives the parent audit one bounded chance to commit after the child audit connection is discarded. The caller's transaction cannot preserve this evidence on rollback.
