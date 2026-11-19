# Ticket-to-Intune workflow

Who does what, in order. The steps in the **Studio** column are buttons the user presses; you do the **Agent** column.

| # | Stage | Who | Output |
|---|-------|-----|--------|
| 1 | Extract links and targets | Studio | `parsed-ticket.json` |
| 2 | Triage the ticket | **Agent** | `products.json` |
| 3 | Create the isolated runtime | Studio | `runtime\` |
| 3b | Find apps that already exist (recipes and Intune), then product routing | Studio and the user | `completedProducts` in `products.json` |
| 4 | Check Intune for existing apps | Studio | `inventory\intune-inventory.json` |
| 5 | Research, author and run safe checks | Recipe Builder | Yardstick development recipe and evidence |
| 6 | Prepare test (installs nothing) | Studio | `artifacts\package-tests\<run>\` |
| 7 | Fix a recipe from test results | Recipe Builder (when needed) | Updated recipe and evidence |
| 8 | Install test, after the user approves it | Studio | `artifacts\package-tests\<run>\` |
| 9 | Build the `.intunewin` packages | Studio | `local-packages\` |
| 10 | Prepare the Intune change | Studio | `intune-upload\live-change-set.json` |
| 11 | Upload and assign, after the user approves it | Studio | `intune-upload\live-change-result.json` |

## 1. Establish the request (your triage)

Preserve the request as received; never edit `ticket.md`. For every product record its canonical name, publisher, edition, architecture and requested version or channel; the links that belong to it; update or admin-intervention requirements; target strings such as asset tags, device names or groups; licensing, authentication, restart, dependency and data-handling constraints; and any ambiguity that would change the package or the target.

Do not infer that links match product names by position unless the ticket or the link's contents establish it. Never include signed query strings or private file-sharing URLs (Dropbox, OneDrive, Google Drive, Box or any other service) in any file; redact the query when you quote a link.

## 2. Inventory comes before packaging

The Studio searches Intune read-only for each product, using your `searchPatterns`, before anything is downloaded. Classify each product from the result:

- **Already available**: an appropriate current app is in Company Portal for the intended audience. Stop work on that product.
- **Existing but not equivalent**: stop for review. Do not suggest forcing an update, replacing, renaming or creating a confusing duplicate.
- **Not present**: continue.
- **Inventory unavailable or ambiguous**: surface the blocker; nothing is uploaded.

## 3. Acquire or request the installer

Prefer an official vendor source with a stable unattended enterprise installer and a verifiable version. Use a supplied file-sharing folder only if the ticket authorises it, and inspect its contents before choosing a payload.

Files the ticket links to are already downloaded into `downloads/` with their facts in `downloads.json`, and after triage the Studio copies each product's files (its `downloads`) to `<SoftwareDropbox>\<recipe-id>\` and lists them on the product as `ticketInstallers`. Recipe authoring can then use that installer through `manualDownload` when the vendor offers no unattended download of the same file.

When no retrievable installer exists, report the staging location `<SoftwareDropbox>\<recipe-id>\` with acceptable file types and names, and stop that product as **blocked awaiting installer**. Do not fabricate a URL or substitute a trial or community edition.

## 4. Research and author

Recipe Builder owns this work through Yardstick's recipe skill and syncs checked recipes back into the ticket. The plugin triage agent never authors or repairs recipes.

## 5. Local validation (the Studio's)

In increasing order of risk the Studio parses and schema-checks the recipe, resolves the payload and records SHA-256, size, version metadata and Authenticode status (Prepare test), and then, only after the user approves the exact recipes and this PC, runs the lifecycle: pre-detect, install, post-detect, uninstall, post-uninstall detect, residue. Read `package-validation.md` for what each outcome means.

## 6. Targeting

The Studio resolves each target to an exact Entra object ID and checks it afresh before it prepares a change. Asset tags or Intune device names such as `NC-4214805` are not assignment groups. If a ticket names only such targets, set `blocked-awaiting-targeting` and say what mapping is needed. Never suggest All Devices or All Users.

## 7. After the change

The Studio reads everything back with fresh requests and records the app and assignment IDs. A partial upload, a mismatched app or a missing assignment is a failure for a person to review. Nothing is cleaned up by deletion.
