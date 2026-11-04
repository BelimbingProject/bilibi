# Documents

Use Documents to keep one business document with every revision and link it to the work it supports.

## Find a document

Use `core.document.list` to search by name or filter by category, type, status or related record. The results include only documents explicitly shared with you or your current workplace. A record link never gives you document access.

Use `core.document.get` for one document, its revisions and its related records. Use the revision's `download_path` only when you have download permission.

## Create and revise

Use `core.document.create` for an internal file already stored through the Documents page, or for an external HTTPS link. Add later versions with `core.document.add_revision` and explain the change in `change_log`.

Use `core.document.update` for the document's name, publication date, category, subcategory, type or status. Link and unlink work records with `core.document.link` and `core.document.unlink`.

## Share safely

Use `core.document.access` to read the current explicit grants. Use `core.document.share` to give an active account or your current workplace view access, with optional download, manage or share permission. Use `core.document.unshare` with the grant ID to remove access; you cannot remove your own grant.

## Delete

Use `core.document.delete` with the document's current version. Documents checks every revision together. Retention or legal hold refuses the deletion and keeps the document with its complete revision history.
