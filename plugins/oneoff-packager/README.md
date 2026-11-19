# One-Off Packager

Packages software requests that are not good candidates for a maintained Yardstick recipe: a ticket with a vendor link, a licensed installer someone supplies, or a one-time need. The plugin agent triages the ticket; the shared Recipe Builder researches, authors, safely checks and repairs each recipe. **Run safe stages** advances the normal path until it needs your judgment or consent; everything that installs, packages or touches Intune remains a visible stage, and risky work waits for approval.

## Set up

In *Settings → Plugins → One-Off Packager → Settings*:

The plugin tab's **Plugin settings** action opens this same global Settings card; configuration is not duplicated inside the plugin page.

| Setting | |
|---------|--|
| Work folder | Where each ticket gets its own folder. |
| YardstickDev folder | A git checkout of YardstickDev. It is only read, and it does not need to be clean: each ticket clones its committed state into a private runtime, so uncommitted changes neither block a ticket nor reach it. The package test runs this checkout's recipe harness, so **Clone private copy** still keeps One-Off work from waiting for Recipe Builder. |
| Tenant ID | Steps that read or change Intune refuse to run unless the stored credential belongs to this tenant. |
| Tenant name | Optional extra check of the tenant's display name. |
| Default target group ID | The static Entra security group apps are made Available to. Never All Users or All Devices. |
| Credential target | The Windows Credential Manager entry Yardstick uses for Graph (default `Yardstick:IntuneGraph`). |

Needs PowerShell 7 and the `powershell-yaml` module (0.4.12 or later). Packaging uses the runtime's own `New-YardstickWin32AppPackage`, which fetches and checks Microsoft's IntuneWinAppUtil the way Yardstick does.

## The pipeline

Use **Run safe stages** to start or resume the normal path. Completed stages are not repeated. The run pauses when a stage fails, while linked Recipe Builder work needs attention, and before the local lifecycle test or live Intune change. Approve and run a guarded stage to continue automatically from the next safe stage. If Studio closes during a stage, that stage is marked **Interrupted** and the run can be continued after you retry it.

After triage, **Find apps that already exist** looks for each product among YardstickDev's shipped recipes (same id or name) and, read-only through a temporary guarded clone of YardstickDev that is deleted straight afterwards, the tenant's Windows apps (the product's search patterns). A product found in either moves to **Already available** under Product routing, marked with what was found; one in Intune without a Yardstick recipe gets an amber **In Intune, no Yardstick recipe** mark. **Package anyway** moves it back, and that choice sticks: the check leaves it in place, and the later Intune change accepts the existing apps it showed you (but still stops on any new match). Without a Tenant ID or a working credential only recipes are checked, and the stage says so.

A ticket with several products, or any product already available, pauses at **Product routing**. Send maintained, reusable applications to Recipe Builder one at a time; they become independent working-list entries with the original request and their triage details. Confirm the products that should remain one-off. After read-only inventory, Studio creates linked Recipe Builder entries for those remaining products too. Their checked development recipes and icons synchronize back into the ticket, and the One-Off pipeline resumes automatically. A routed or already-available product can be moved back until the isolated runtime is created. If every product is routed or already available, the One-Off pipeline completes after you confirm, without running packaging steps.

1. **Extract links and targets** reads the request into `parsed-ticket.json`.
2. **Download linked installers** fetches every file the ticket links to into `downloads\` (sharing links from Dropbox, OneDrive/SharePoint, Google Drive, Box and GitHub are turned into direct downloads, other links are fetched as they are, zips unpacked; a link that only opens a web page, such as a service that needs signing in, is reported and skipped) and records each file's size, SHA-256, Authenticode signer, version resources and MSI properties in `downloads.json` and the **Downloads** panel. Nothing downloaded is run, and link query strings (access tokens) are never written down.
3. **Triage** (agent) writes `products.json`: each product, its links, targets, the patterns that would find it in Intune, and which downloaded files belong to it. It uses the files' own metadata to identify the product and version.
4. **Put installers in the software dropbox** copies each product's downloads to `<SoftwareDropbox>\<product id>\` (the `SoftwareDropbox` in the YardstickDev folder's preferences), where Yardstick's `manualDownload` recipes look for them, and lists them on the product as `ticketInstallers` for recipe authoring. A different file already there is never replaced; without a `SoftwareDropbox` the files stay in the ticket.
5. **Find apps that already exist** marks products that already have a Yardstick recipe or an Intune app and moves them to **Already available**, before Product routing (above).
6. **Create the isolated runtime** clones YardstickDev's committed state (`git clone` of the checked-out commit; uncommitted changes are left out and counted in the stage summary) into `runtime\`, copies in the git-ignored files it needs (preferences, the icon library, `*.common.yaml` recipe data), adds a guard that blocks every Graph `DELETE`, then proves it.
7. **Check Intune for existing apps** is a read-only search, before anything is packaged.
8. **Recipe Builder** researches, authors and runs safe checks for each remaining product. Its normal test-and-repair loop handles recipe failures. Studio copies only the checked development recipe and its icon back into this ticket.
9. **Prepare test** downloads and checks the installer and generates the scripts. It installs nothing.
10. **Write the install-test request**, then **Install, detect and uninstall on this PC**: the real lifecycle test. You approve the request file's exact bytes, which name this PC and the exact recipes; a recipe changed afterwards voids the approval.
11. **Build the packages** from the installer that passed the test.
12. **Prepare the Intune change** re-checks the tenant and group and writes the change set and a summary. It changes nothing.
13. **Upload and assign** runs only for the change set you approved. It creates each app, verifies it with fresh reads, then makes it Available to the group. It can be run again under the same approval to finish an interrupted change, without uploading an app twice.
14. **Clean up the runtime** deletes the ticket's YardstickDev clone once the Intune change is done, keeping its record (source commit, guard hash) as `artifacts\runtime-manifest.json`. A failed or interrupted change keeps the clone so it can be finished.

**Import…** reads a ServiceNow export, the packaging handoff or a CSV into tickets.

Recipe Builder's contextual extractor can recommend this plugin for licensed, supplied-media or genuinely one-time requests. Sending the item creates a ticket here with the original request intact. Product routing separates reusable applications, while linked authoring keeps One-Off products in this ticket and uses Recipe Builder only for the recipe work.

## Guarantees

- No step ever sends a Graph `DELETE`, creates an uninstall assignment, or targets All Users or All Devices.
- `allowAvailableUninstall` is `true` unless a change set says otherwise.
- The agent works only inside the ticket's folder and cannot run these steps.
- Nothing is called successful on an exit code alone: results are read back from Intune.

A failed upload can leave an incomplete app in Intune for you to review, because deletion is blocked.

## Tests

`.\build.ps1 -Task plugin-test` from the Studio's repository runs the Pester tests in `tests\`.
