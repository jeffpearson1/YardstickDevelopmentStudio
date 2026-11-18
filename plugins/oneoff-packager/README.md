# One-Off Packager

Packages software requests that are not good candidates for a maintained Yardstick recipe: a ticket with a vendor link, a licensed installer someone supplies, or a one-time need. The plugin agent triages the ticket; the shared Recipe Builder researches, authors, safely checks and repairs each recipe. **Run safe stages** advances the normal path until it needs your judgment or consent; everything that installs, packages or touches Intune remains a visible stage, and risky work waits for approval.

## Set up

In *Settings → Plugins → One-Off Packager → Settings*:

The plugin tab's **Plugin settings** action opens this same global Settings card; configuration is not duplicated inside the plugin page.

| Setting | |
|---------|--|
| Work folder | Where each ticket gets its own folder. |
| YardstickDev folder | A **clean git checkout** of YardstickDev. Use **Clone private copy** to give the plugin its own checkout; it is only read, each ticket gets a private runtime, and file-only One-Off work no longer waits for Recipe Builder to release its checkout. |
| Tenant ID | Steps that read or change Intune refuse to run unless the stored credential belongs to this tenant. |
| Tenant name | Optional extra check of the tenant's display name. |
| Default target group ID | The static Entra security group apps are made Available to. Never All Users or All Devices. |
| Credential target | The Windows Credential Manager entry Yardstick uses for Graph (default `Yardstick:IntuneGraph`). |

Needs PowerShell 7 and the `powershell-yaml` module (0.4.12 or later). Packaging uses the runtime's own `New-YardstickWin32AppPackage`, which fetches and checks Microsoft's IntuneWinAppUtil the way Yardstick does.

## The pipeline

Use **Run safe stages** to start or resume the normal path. Completed stages are not repeated. The run pauses when a stage fails, while linked Recipe Builder work needs attention, and before the local lifecycle test or live Intune change. Approve and run a guarded stage to continue automatically from the next safe stage. If Studio closes during a stage, that stage is marked **Interrupted** and the run can be continued after you retry it.

After triage, a ticket with several products pauses at **Product routing**. Send maintained, reusable applications to Recipe Builder one at a time; they become independent working-list entries with the original request and their triage details. Confirm the products that should remain one-off. After read-only inventory, Studio creates linked Recipe Builder entries for those remaining products too. Their checked development recipes and icons synchronize back into the ticket, and the One-Off pipeline resumes automatically. A routed product can be returned until the isolated runtime starts. If every product is routed, the One-Off pipeline completes without running packaging steps.

1. **Extract links and targets** reads the request into `parsed-ticket.json`.
2. **Triage** (agent) writes `products.json`: each product, its links, targets and the patterns that would find it in Intune.
3. **Create the isolated runtime** copies YardstickDev into the ticket with a guard that blocks every Graph `DELETE`, then proves it.
4. **Check Intune for existing apps** is a read-only search, before anything is downloaded.
5. **Recipe Builder** researches, authors and runs safe checks for each remaining product. Its normal test-and-repair loop handles recipe failures. Studio copies only the checked development recipe and its icon back into this ticket.
6. **Prepare test** downloads and checks the installer and generates the scripts. It installs nothing.
7. **Write the install-test request**, then **Install, detect and uninstall on this PC**: the real lifecycle test. You approve the request file's exact bytes, which name this PC and the exact recipes; a recipe changed afterwards voids the approval.
8. **Build the packages** from the installer that passed the test.
9. **Prepare the Intune change** re-checks the tenant and group and writes the change set and a summary. It changes nothing.
10. **Upload and assign** runs only for the change set you approved. It creates each app, verifies it with fresh reads, then makes it Available to the group. It can be run again under the same approval to finish an interrupted change, without uploading an app twice.

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
