# Dashboard agent operations

This module contributes the connector and Customisations door for [Dashboard arrangement](../dashboard/README.md#agent-operations). Dashboard owns the account settings, placement rules and provider access checks. Session and performance widgets depend on Dashboard, so the agent contributions sit above both Dashboard and Agent API.

The operation keys are `base.dashboard_agent.read`, `base.dashboard_agent.set` and `base.dashboard_agent.restore`. Writes use the current version from the read and the signed-in account's own settings.
