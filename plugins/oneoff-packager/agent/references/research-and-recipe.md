# Research and recipe guidance

## Source priority

Use current primary sources: the vendor's download, deployment, release-note, checksum and licensing pages; official repositories and releases for open-source products; Microsoft documentation for Intune behaviour. Search snippets, software aggregators, forums and copied ticket text are discovery leads only.

Establish the exact Windows product and edition, supported architecture, the latest appropriate stable version, installer technology, silent install and uninstall commands, exit and reboot codes, prerequisites, system or user context, update behaviour, licence terms and a durable detection signal. Cite the pages you used in `research-and-status.md`.

Never substitute similarly named software. Examples that need disambiguation: "Recover My Files" versus other recovery products; "Volatility" versus Volatility 3 standalone builds.

## Payload checks

Before anything runs, and whenever you can see a file under your approvals:

- capture the final resolved source URL with secrets and query values removed;
- record the file name, length, SHA-256, PE or MSI version metadata and Authenticode signer and status;
- compare published checksums or signatures when the vendor supplies them;
- reject HTML or error pages posing as installers and unexpected nested executables;
- list archives without running their contents;
- flag unsigned installers and signer/publisher mismatches for explicit review.

A private Dropbox link authorises retrieval only for the stated task. Do not republish it or assume it stays valid. For a folder link, inventory the folder and map each payload by contents and metadata, never by URL order.

## Recipe decisions

`runtime\Docs\WritingRecipes.md` and the templates in `runtime\Templates\` are authoritative over this summary.

- Prefer MSI product-code detection when it is stable and appropriate. Otherwise use a version-aware file, registry or script rule. Detection must be true only for the intended product at an acceptable version.
- Make install and uninstall silent, non-interactive, bounded and idempotent where possible.
- Respect a ticket's requirement that updates need no admin intervention: find out whether the vendor's self-update works in the chosen context, or rely on later Intune supersedence. Do not just repeat the claim.
- Use `manualDownload: true` when a licensed or authenticated payload has to be staged in `SoftwareDropbox\<recipe-id>`. Derive `$version` and `$fileName` from the inspected payload and fail on zero or ambiguous candidates.
- Keep descriptions factual. No ticket numbers, private URLs, secrets or requester information in app metadata.
- Use a correctly licensed PNG or JPG icon within Intune's limits. Do not scrape copyrighted branding.

For a recipe that already exists in `runtime\Recipes\`, treat it as evidence, not approval. Confirm that its edition, source, commands, detection and update behaviour match this ticket and the current installer before you adapt it. Copy it to `recipes\` and change the copy.
