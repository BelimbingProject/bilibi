# ADR 34: One-shot model calls through configured providers

**Document Type:** Architecture Decision Record
**Status:** Accepted; implemented
**Agents:** claude-code/opus-5.5
**Scope:** When Bilibi itself may call a model, and the one small service every such call goes through
**Last Updated:** 2026-11-19

> The contract is [`apps/base/ai/README.md`](../../../apps/base/ai/README.md). This record amends one sentence of [ADR 01](./01-agent-first-operations.md) ("Bilibi runs no model").

## Context

ADR 01 keeps the model in the person's harness: an agent reads, drafts and asks for approval, and Bilibi runs no model. Some help only makes sense inside a page, at one click, for a person with no harness open: a summary of a long record, a first draft of a reply. Without a shared service, each module that wants this would call a model's API itself, with its own credentials, switches and logging.

The owner decided on 2026-11-19 to keep this small: a Base service for one-shot calls, with anything more in Domains and Extensions.

## Decision

- **Bilibi makes simple one-shot model calls through configured providers, never agent loops.** A module sends instructions plus text to a named provider, or the default, and gets text back or a plain "unavailable" answer. There is no tool calling, no multi-step loop, no streaming and no embedding.
- **One service, `base/ai`.** No Domain, Extension or Core module calls a model's HTTP API itself.
- **Providers are settings, not code.** An operator lists providers in System Settings: a name, an OpenAI-compatible base URL and a model, with an optional API key that is encrypted and never shown back, not even through the stored-secret reveal. Several are allowed. The service sends through `base/integration`, the only HTTP client.
- **Hosted providers are public HTTPS; local ones are an explicit choice.** A provider reaches a private address (a local Ollama, vLLM or LM Studio) only when the operator marks it local. Every other provider goes through Integration's public-HTTPS path, so a hosted name cannot resolve to an internal service.
- **Off by default.** AI works only when the installation switch and the company's switch are both on.
- **Each call is logged as metadata:** who asked, the calling module, the provider, input and output size, the outcome and the duration. Instructions, text and replies are never stored.
- **Unavailable is an answer, not an error.** AI switched off, no provider, a timeout or a provider failure all return "unavailable", so the page carries on without AI. Every call has a deadline bounded by a setting.
- **Output is untrusted text.** It is shown escaped and never applied, sent or published without a person's reviewed write through the owner's operation contract.

## Not in scope

- **Features:** summaries, drafts and bots belong to the Domains that need them.
- **Vendor-specific providers:** an API that is not OpenAI-compatible belongs to an Extension.

## Prompt injection

Text sent with a call may contain instructions written by someone else. ADR 01's position stands: there is no general solution. These rules limit the damage: the module's instructions go in the system message and the text in the user message; there are no tools for the model to call; and output is never acted on without a person's review.

## Alternatives rejected

- **Each module calls a provider directly.** That means credentials, switches and logs in every module, and no single place to turn AI off.
- **Providers registered in code as a port or registry.** Adding a provider would mean a release. Settings let an operator point at a local or hosted server today.
- **Only the harness runs models.** That leaves no in-page help for people without a harness open.

## Consequences

- ADR 01's context now names this exception.
- A Domain that wants in-page AI adds a call through `base/ai` and hides its action when `availability` says AI is unavailable.
- Operators get one switch per installation and per company, one place to configure providers, and the AI Interactions log under System.
