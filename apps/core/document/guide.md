# Documents

Use Documents to keep one business document with every revision and link it to the work it supports.

## Find a document

Use `core.document.list` to search by name or exact integration sync ID or filter by category, type, status or related record. The results include only documents explicitly shared with you. A record link never gives you document access.

Use `core.document.get` for one document, its revisions and its related records. Use the revision's `download_path` only when you have download permission.

## Create and revise

Use `core.document.create` for an internal file already stored through the Documents page, or for an external HTTPS link. Add later versions with `core.document.add_revision` and explain the change in `change_log`.

Use `core.document.update` for the document's name, publication date, category, subcategory, type, status, integration sync ID or document relation. The sync ID is unique within the tenant. A relation needs a document ID you can view and a kind (supersedes or replaces); clear both fields together. `core.document.add_revision` accepts an optional label and an earlier revision ID from the same document in replaces_revision_id. The revision IDs are returned by `core.document.get`. Link and unlink work records with `core.document.link` and `core.document.unlink`.

## Favourites, duplicates, copy and merge

Use `core.document.mark` to favourite or follow a document: marks are yours alone, `core.document.list` takes `mark` (`favourites` or `following`), and followers are notified of new revisions. Use `core.document.duplicates` to find documents you may see with the same file, link or name. Use `core.document.copy` for a new document with only the latest revision: a link copies directly; a file needs the Documents page's Copy action to store the file first. Use `core.document.merge` to fold duplicates into a survivor with each document's stored `version`: it needs merge, manage and share permission on every document, moves each duplicate's revisions after the survivor's latest, combines links, marks and access, and removes the duplicates.

## Share safely

Use `core.document.access` to read the current explicit grants. Use `core.document.share` to give an active account view access, with optional download, manage or share permission, `until` (the last day it works, UTC) and `email` (queue an email with a link that opens the document only for their account; it is audited and limited, and the result says `rate_limited` when a limit stopped it). Use `core.document.unshare` with the grant ID to remove access; you cannot remove your own grant.

## Expiry and custom fields

Set `expires_on` in `core.document.create` or `core.document.update` (an empty string clears it): you get a reminder task through Core Activity, due the tenant's lead time before, and the list filters `expiry` (`expired` or `soon`). Tenant-defined custom fields travel as `custom` `{key, value}` pairs on create and in `patch`; `core.document.get` returns them with labels and types.

## Delete

Use `core.document.delete` with the document's current version. Documents checks every revision together. Retention or legal hold refuses the deletion and keeps the document with its complete revision history.
