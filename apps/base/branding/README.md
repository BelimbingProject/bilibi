# Branding

**Use it for:** showing the installation's own name, logo and accent colour in place of Bilibi's (`Branding::current`), and reading which brand a page carries.

**Not for:** a workspace's own email sender name (`mail.sender_name`, `base/mail`), a customer portal's title and logo (`core/portal`), or a company's image in the top bar (`core/company`).

Set the brand once for all workspaces, including staff sign-in before a workspace is selected.

## Settings

Three installation (global) settings hold the brand. Nothing set means Bilibi's own.

| Setting | Default | Meaning |
| --- | --- | --- |
| `branding.name` | Blank (Bilibi) | Display name, up to 40 characters, with no surrounding whitespace, control characters, `@`, double quotes or angle brackets |
| `branding.logo` | None | Media asset ID of a PNG, JPEG or WebP image of at most 1 MiB, in the platform operator's workspace |
| `branding.accent` | `lime` | `lime`, `teal`, `cyan`, `indigo`, `violet` or `fuchsia` |

- **Accent is a fixed set.** Pages allow no inline style, and each palette is checked in the light and dark themes. None is a status colour. The palettes live in `apps/base/ui/assets/css/app.css` (Adopter accent).

## Where it shows

| Place | With a brand | Without |
| --- | --- | --- |
| Staff sign-in, recovery, invitation, 2FA and review cards | Available logo and configured name | The Bilibi wordmark |
| Workspace top bar | Available logo and configured name, linked to the dashboard | The Bilibi wordmark |
| Browser tab | "page · name", the available logo, an accent-coloured icon for a custom name, or the default icon | "page · Bilibi", the Bilibi mark |
| Staff error pages outside debug diagnostics | The same logo and name | The Bilibi wordmark |
| Buttons, current menu item, selections | The accent | Lime |
| Account menu | "Powered by Bilibi" | "Powered by Bilibi" |

Set the name and logo independently. A name alone draws a text wordmark. An available logo alone draws the image and leaves the default product name in tab titles and accessible labels. An accent on its own keeps the default wordmark and tab icon. The customer portal keeps its workspace title and logo. See [Mail sender names](../mail/README.md#sender-name) for email.

## Change it

| Who | How |
| --- | --- |
| An agent | `base.branding.get`, `base.branding.set` (`name`, `accent`, `logo_asset_id`) and `base.branding.restore` (`item`: `name`, `logo` or `accent`). Writes are reviewed first. The guide is `base.branding` |
| A person | **System Settings → Upload logo** stages an image through Media and returns its asset ID for the reviewed set operation |
| Inspection | **Administration → Customisations → Brand** lets branding managers inspect changed settings, including an unavailable logo |

- Changing it needs `admin.system.branding.manage`, a platform capability: only the platform operator's administrators hold it. `base.branding.get` is open to every signed-in person, as the brand is on the sign-in page; `can_manage` says whether the caller may change it.
- An agent names a logo already stored in Media; it cannot send image bytes. The image must be the operator workspace's own and must have passed its file check.
- A replaced logo's file stays in Media until an operator removes it.
- The Customisations rows show no change time: the three settings carry no revision of their own. The audit log has each change (kind `base/settings`).

## Read it (host)

`Branding::current()` returns a `bilibi_ui::Brand` for one render and never fails a page: a failed read is logged and the page carries Bilibi's brand. The Web host passes it to the shell (`ShellContext::brand`), the credential layout (`AuthLayout::brand`), the standalone review, the error document and `Document`. A page never reads the brand or draws a logo itself; it calls `ui::wordmark`.

A logo link is a short-lived signed Media link for the operator workspace's asset (`Media::tenant_stream_url`). A logo that cannot be served is left out. [Media's signed stream contract](../media/README.md#signed-streams) governs public image delivery.
