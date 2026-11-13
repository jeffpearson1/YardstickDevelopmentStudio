Read `ticket.md`, `parsed-ticket.json` and anything in `intake/`. Then write `products.json`: one entry for each separate piece of software the ticket asks for.

First read `.studio\agent\references\workflow.md`, section 1.

## How to read the ticket

- Decide the products yourself from the ticket text. The `ProductsHeuristic` list in `parsed-ticket.json` is only a guess.
- Do not assume links match product names by position. Match a link to a product only when the ticket or the link's own contents say so. A folder link may hold several products; list what is in it only if you can see its contents without downloading installers.
- Never copy a private link's query string into any file. Keep the host and path; replace the query with `(query removed)`.
- Do not substitute similarly named software. Where two products share a name, record both readings in `ambiguities`.
- Targets such as asset tags (`NC-1234567`), device names or user names are **not** group names. Record them under `targets`. Only a real Entra group object ID belongs in `requestedGroupId`, and only if the ticket names one.

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
- `searchPatterns`: case-insensitive regular expressions that would match this product's Intune app, by its name or publisher, including likely older names and spellings. Make them specific enough not to match unrelated apps (a bare `viewer` is too broad). A Studio step searches the tenant with these, so a pattern that is too narrow can miss an existing app.
- `requestedGroupId`: `null` unless the ticket names an approved Entra group by object ID.
- Leave `status` as `needs-research` for every product at this stage.

Write `products.json` and nothing else. If the ticket contains no software request, write `{ "products": [] }` and say why in your report.
