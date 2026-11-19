Read `ticket.md`, `parsed-ticket.json`, `downloads.json` (if present) and anything in `intake/`. Then write `products.json`: one entry for each separate piece of software the ticket asks for.

If `products.json` already contains `routedProducts` or `completedProducts`, preserve those arrays exactly and do not put those product IDs back into `products`. The user routed the first to Recipe Builder; the second already exist as a Yardstick recipe or Intune app. Keep `completedOverride`, `existing` and `flags` on any product that has them.

First read `.studio\agent\references\workflow.md`, section 1.

## How to read the ticket

- Decide the products yourself from the ticket text. The `ProductsHeuristic` list in `parsed-ticket.json` is only a guess.
- Do not assume links match product names by position. Match a link to a product only when the ticket or the link's own contents say so. A folder link may hold several products; list what is in it only if you can see its contents without downloading installers.
- Never copy a private link's query string into any file. Keep the host and path; replace the query with `(query removed)`.
- Do not substitute similarly named software. Where two products share a name, record both readings in `ambiguities`.
- Targets such as asset tags (`NC-1234567`), device names or user names are **not** group names. Record them under `targets`. Only a real Entra group object ID belongs in `requestedGroupId`, and only if the ticket names one.

## Installers the ticket's links delivered

The Studio has already downloaded every file the ticket links to into `downloads/` (zips unpacked) and listed them in `downloads.json`: each file's path, size, SHA-256, Authenticode signer and, where present, its version resources and MSI properties (`ProductName`, `ProductVersion`, `Manufacturer`, `ProductCode`). Links that opened a web page or failed are listed with why. Use these facts:

- to tell which product a file is, and its exact publisher and version, when the ticket is vague. The file's own metadata outranks its file name and the link's position in the text;
- to fill `version` from the file when the ticket names none but supplied this exact installer;
- to note anything packaging needs (unsigned file, an MSI vs. an EXE wrapper, several architectures, a zip of several installers) in `notes`.

List the files that belong to each product in its `downloads`, as the work-item-relative paths from `downloads.json` (for example `downloads\Zoom\ZoomInstallerFull.msi`; escape the backslashes in JSON). A later Studio step copies them to the software dropbox under the product id for the recipe to use. Leave out files that are not installers for that product (readme files, other products' installers). Do not run, install or unpack anything yourself.

## If the ticket came from SCCM

When `intake\sccm-export.json` exists the request is one application: write exactly one product. Take the name, publisher and version from the export; there are no links (leave `links` empty) and the deployment collections are not `targets` for Intune. Put the application's name and manufacturer in `searchPatterns` so the existing-apps check finds a copy that was migrated before.

## The file

```json
{
  "products": [
    {
      "id": "faststoneimageviewer",
      "name": "FastStone Image Viewer",
      "publisher": "FastStone Corporation",
      "version": "8.5",
      "status": "needs-research",
      "notes": "One or two sentences: edition, architecture, anything the packager needs.",
      "links": ["https://example.com/FSViewerSetup85.exe (query removed)"],
      "downloads": ["downloads\\FSViewerSetup85.exe"],
      "targets": ["NC-4214805"],
      "searchPatterns": ["fast\\s*stone", "image\\s*viewer"],
      "requestedGroupId": null,
      "ambiguities": []
    }
  ]
}
```

- `id`: the recipe id. Lowercase letters, digits, dots, dashes and underscores; start from the product name with spaces removed.
- `version`: what the ticket asks for, or `""` if it names none. Do not guess one here.
- `searchPatterns`: case-insensitive regular expressions that would match this product's Intune app, by its name or publisher, including likely older names and spellings. Make them specific enough not to match unrelated apps (a bare `viewer` is too broad). A Studio step searches the tenant with these before routing and moves a product with a match to Already available, so a pattern that is too narrow misses an existing app and one that is too broad hides a product that needs packaging.
- `requestedGroupId`: `null` unless the ticket names an approved Entra group by object ID.
- Leave `status` as `needs-research` for every product at this stage.

Write `products.json` and nothing else. Preserve `routedProducts` and `completedProducts` when they exist. If the ticket contains no software request, write `{ "products": [] }` and say why in your report.
