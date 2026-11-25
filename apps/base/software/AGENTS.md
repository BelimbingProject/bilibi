Wake the privileged installer only through the per-job marker contract in “How a worker plugs in” in the module README; application code must never control the systemd unit directly, because the shared incoming folder is the privilege boundary.

Use the release discovery result in `src/update.rs` for both owner notices and agent reads, so stale, partial and ambiguous listings give the same answer everywhere. The exact install review and proof checks remain authoritative; see “Update notices” in the module README.

Pass the preview’s SQL client through release discovery and install review. The connection pool rejects a second checkout from the same task. See the write handler in `src/agent.rs`.
