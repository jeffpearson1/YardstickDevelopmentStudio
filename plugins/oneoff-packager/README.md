# One-Off Packager

Packages software requests that are not good candidates for a Yardstick recipe: a ticket with a vendor link, a licensed installer someone supplies, a one-time need. An agent (GitHub Copilot CLI or Claude Code, the one you picked in Settings) does the reading and research; everything that installs, packages or touches Intune is a separate step with its own button, and the risky ones wait for your approval.

## Set up

In *Settings → Plugins → One-Off Packager → Settings*:

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

1. **Extract links and targets** reads the request into `parsed-ticket.json`.
2. **Triage** (agent) writes `products.json`: each product, its links, targets and the patterns that would find it in Intune.
3. **Create the isolated runtime** copies YardstickDev into the ticket with a guard that blocks every Graph `DELETE`, then proves it.
4. **Check Intune for existing apps** is a read-only search, before anything is downloaded.
5. **Research** (agent) writes a recipe, an icon and `research-and-status.md` for each product that is not already in Intune.
6. **Prepare test** downloads and checks the installer and generates the scripts. It installs nothing.
7. **Fix a recipe** (agent) reads the test evidence and corrects the recipe.
8. **Write the install-test request**, then **Install, detect and uninstall on this PC**: the real lifecycle test. You approve the request file's exact bytes, which name this PC and the exact recipes; a recipe changed afterwards voids the approval.
9. **Build the packages** from the installer that passed the test.
10. **Prepare the Intune change** re-checks the tenant and group and writes the change set and a summary. It changes nothing.
11. **Upload and assign** runs only for the change set you approved. It creates each app, verifies it with fresh reads, then makes it Available to the group. It can be run again under the same approval to finish an interrupted change, without uploading an app twice.

**Import…** reads a ServiceNow export, the packaging handoff or a CSV into tickets.

Recipe Builder's contextual extractor can recommend this plugin for licensed, supplied-media or genuinely one-time requests. Sending the item creates a ticket here with the original request intact. If research shows it should become a maintained recipe after all, **Continue in Recipe Builder** sends the context back without deleting this work item.

## Guarantees

- No step ever sends a Graph `DELETE`, creates an uninstall assignment, or targets All Users or All Devices.
- `allowAvailableUninstall` is `true` unless a change set says otherwise.
- The agent works only inside the ticket's folder and cannot run these steps.
- Nothing is called successful on an exit code alone: results are read back from Intune.

A failed upload can leave an incomplete app in Intune for you to review, because deletion is blocked.

## Tests

`.\build.ps1 -Task plugin-test` from the Studio's repository runs the Pester tests in `tests\`.
