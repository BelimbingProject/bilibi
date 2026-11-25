Use the release discovery result in `src/update.rs` for both owner notices and agent reads, so stale, partial and ambiguous listings give the same answer everywhere. The exact install review and proof checks remain authoritative; see “Update notices” in the module README.

Wake the privileged installer only through the per-job marker contract in “How a worker plugs in” in the module README; application code must never control the systemd unit directly, because the shared incoming folder is the privilege boundary.
