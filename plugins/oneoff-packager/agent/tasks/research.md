Research each product in `products.json`, and for each one that is not already in Intune write a Yardstick recipe, an icon and the research notes.

Read `.studio\agent\references\research-and-recipe.md` and `yardstick-integration.md` first, then `runtime\Docs\WritingRecipes.md` and the matching template in `runtime\Templates\`.

## 1. Use the inventory

Read `inventory\intune-inventory.json`. For each product, look at `matchingApps` whose `matchedPatterns` include that product's `searchPatterns`:

- An appropriate, current app already available to the intended audience: set the product's `status` to `already-available`, note the app ID, and **write no recipe**.
- An app that is similar but not clearly equivalent (different edition, version, publisher or detection): set `existing-not-equivalent` and write no recipe. The user decides.
- Nothing matches: carry on.

## 2. Find the installer

Prefer an official vendor download with a stable unattended installer and a verifiable version. Use a link from the ticket only if it is clearly the right product. If no retrievable installer exists, set `blocked-awaiting-installer`, write in `research-and-status.md` the folder `<SoftwareDropbox>\<recipe-id>\` where one should be staged and the file types you would accept, and **do not fabricate a URL or use a trial or community edition**. A recipe that needs a staged payload uses `manualDownload: true`.

You may only look at downloaded files with the tools your approvals allow (see the approvals section of your prompt). Without the download approval, work from the vendor's documentation and finish with `STUDIO-NEEDS-APPROVAL: downloads` if you need the file itself.

## 3. Write the recipe

Write `recipes\<id>.yaml` in the format `WritingRecipes.md` describes. In particular:

- Prefer MSI product-code detection when stable; otherwise a version-aware file, registry or script rule that is true only for this product at an acceptable version.
- Silent, non-interactive, bounded install and uninstall commands taken from the vendor's documentation, never guessed from a different installer technology.
- `displayName`, `publisher` and a factual `description`. No ticket numbers, private links, secrets or requester names.
- An icon in `icons\<id>.png` (a licensed PNG or JPG within Intune's limits); set `iconFile`. Do not scrape branding from an unapproved source.
- Record the architecture, minimum OS and install context honestly in the recipe's fields.

Check each recipe with the runtime's schema validator: `runtime\Tests\RecipeHarness\Test-RecipeSchema.ps1 -RecipePath recipes\<id>.yaml`. Do not run any install test or the lab. The Studio's **Prepare test** step does the download, hash and signature checks and generates the scripts, and the **install test** is the user's decision.

## 4. Write the notes

`research-and-status.md` is what the user reads before approving anything. For each product give: the sources you used (without private query strings), the decisions and assumptions, the installer's version, hash and signature if you saw them, the warnings (unsigned installer, licence terms, restart behaviour), and the status. Update `products.json` `status` to match.

When you are done, list each product with its status and what the user must do next.
