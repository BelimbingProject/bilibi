# Branding

**Use it for:** showing the installation's own name, logo and accent colour in place of Bilibi's (`Branding::current`), and reading which brand a page carries.

**Not for:** a workspace's own email sender name (`mail.sender_name`, `base/mail`), a customer portal's title and logo (`core/portal`), or a company's image in the top bar (`core/company`).

An adopter shows its own brand on staff sign-in, the top bar, tab titles, error pages and email. Bilibi stays as one quiet line, "Powered by Bilibi", in the menu that opens from the account circle.

## Settings

Three installation (global) settings hold the brand. Nothing set means Bilibi's own.

| Setting | Default | Meaning |
| --- | --- | --- |
| `branding.name` | Blank (Bilibi) | Display name, up to 40 characters, without `@`, quotes or angle brackets |
| `branding.logo` | None (the Bilibi mark) | Media asset ID of a PNG, JPEG or WebP image of at most 1 MiB, in the platform operator's workspace |
| `branding.accent` | `lime` | `lime`, `teal`, `cyan`, `indigo`, `violet` or `fuchsia` |

- **One brand per installation.** Every workspace of the installation shows it; sign-in has no workspace yet.
- **Accent is a fixed set.** Pages allow no inline style, and each palette is checked in the light and dark themes. None is a status colour. The palettes live in `apps/base/ui/assets/css/app.css` (Adopter accent).

## Where it shows

| Place | With a brand | Without |
| --- | --- | --- |
| Staff sign-in, recovery, invitation, 2FA and review cards | Logo, then the name | The Bilibi wordmark |
| Workspace top bar | Logo and name; opens the dashboard | The Bilibi wordmark |
| Browser tab | "page · name", the logo as the tab icon | "page · Bilibi", the Bilibi mark |
| Error pages | The same logo and name | The Bilibi wordmark |
| Email | The name as the sender name | `MAIL_FROM_NAME` |
| Buttons, current menu item, selections | The accent | Lime |
| Account menu | "Powered by Bilibi" | "Powered by Bilibi" |

- A workspace's own **Sender name** (`mail.sender_name`) still wins on that workspace's email.
- The customer portal keeps each workspace's own title and logo.
- Email is plain text and shows no logo.

## Change it

| Who | How |
| --- | --- |
| An agent | `base.branding.get`, `base.branding.set` (`name`, `accent`, `logo_asset_id`) and `base.branding.restore` (`item`: `name`, `logo` or `accent`). Writes are reviewed first. The guide is `base.branding` |
| A person | **System Settings → Upload logo** stages an image through Media and returns its asset ID for the reviewed set operation |
| Inspection | **Administration → Customisations → Brand** shows what was configured, including an unavailable logo |

- Changing it needs `admin.system.branding.manage`, a platform capability: only the platform operator's administrators hold it. `base.branding.get` is open to every signed-in person, as the brand is on the sign-in page; `can_manage` says whether the caller may change it.
- An agent names a logo already stored in Media; it cannot send image bytes. The image must be the operator workspace's own and must have passed its file check.
- A replaced logo's file stays in Media until an operator removes it.
- The Customisations rows show no change time: the three settings carry no revision of their own. The audit log has each change (kind `base/settings`).

## Read it (host)

`Branding::current()` returns a `bilibi_ui::Brand` for one render and never fails a page: a failed read is logged and the page carries Bilibi's brand. The Web host passes it to the shell (`ShellContext::brand`), the credential layout (`AuthLayout::brand`), the standalone review, the error document and `Document`. A page never reads the brand or draws a logo itself; it calls `ui::wordmark`.

A logo link is a short-lived signed Media link for the operator workspace's asset (`Media::tenant_stream_url`). A logo that cannot be served is left out.
