# SCCM App Migration

Browse the applications in a Configuration Manager site and move one to Intune. It reads the site through the SMS Provider (WMI over CIM) and **never changes it**: every query is a `SELECT`, and no ConfigMgr console or module is needed.

For each application it shows what SCCM says (deployment types, install and uninstall commands, install context, detection, where it is deployed) and works out what an Intune app would need. Then you pick the route for that application:

- **Package as one-off** starts a ticket in the One-Off Packager with the export and, if you copied it, the application's content as the payload. The agent drafts the recipe from it; the install test, packaging and Intune upload are the One-Off Packager's steps, behind its approvals.
- **Draft a Yardstick recipe** opens the New recipe dialog with the application's details, for the recipe agent to research. Choose this when the vendor offers a download of the version, because a recipe that fetches it is easier to keep up to date.

Needs PowerShell 7 and the One-Off Packager plugin (1.0.1 or later).

## Set up

In *Settings → Plugins → SCCM App Migration → Settings*:

| Setting | |
|---------|--|
| SMS Provider server | The server that runs the SMS Provider, usually the site server. A name, not a URL. |
| Site code | Three characters. |
| Connection protocol | `Dcom` (default) works against most sites. Use `Wsman` if DCOM is blocked and WinRM to the provider is open. |
| Work folder | Each exported application gets a folder here. |
| Largest content to copy (MB) | Copying more than this is refused. |

You connect as yourself (Kerberos), so your account needs to be allowed to read applications in the site (for example through the *Read-only Analyst* security role) and to read the content share. If the connection check fails with an access error, that is the first thing to ask the site's administrators about.

## Using it

1. **Search** lists the latest revision of each application whose name or manufacturer contains the text. An empty search lists them all (the first 300).
2. **Export details** reads the selected application and writes its folder: `export.md` (the description a packager or agent reads), `sccm-export.json` (everything parsed), `intune-mapping.json` and the icon.
3. **Copy content** copies the deployment type's content from its share into the folder's `source\`, verifying every file by SHA-256 and recording each file's signature and version in `source-manifest.json`. It refuses if SCCM now says the content is somewhere else than when you exported, if the content is over the size limit, or if `source\` already exists. It asks you to confirm first.

## What the Intune mapping covers

| SCCM | Intune |
|------|--------|
| Registry setting compared with a value (string, integer, version) | Registry detection rule |
| Registry key or value exists | Registry detection: exists |
| File or folder exists; file version compared | File detection rule |
| MSI product code | MSI detection rule |
| PowerShell detection script | Script detection (the same exit-code-plus-output rule) |
| Rules joined by **AND** | Several rules, all of which must match |
| **OR**, NOT, "does not exist" | Not mapped: Intune detection rules must all match. Rewrite as a PowerShell script. |
| VBScript or JScript detection | Not mapped: Intune runs PowerShell only. |
| Requirement rules, dependencies, supersedence | Listed, not migrated. |
| An application with several deployment types | You choose one; an Intune app has one install command. |

An application whose detection does not map fully is shown as `partial` or `unmapped`, with the reason for each rule. Command lines are shown with passwords, license keys and similar values removed.

The parser follows the structure of `SDMPackageXML`, and its detection handling was written against a real captured detection method plus synthetic digests for the other cases. Check an unusual application's export against the console before relying on its mapping.

## Tests

`.\build.ps1 -Task plugin-test` from the Studio's repository runs the Pester tests in `tests\`, including one that fails if any script calls a cmdlet that could change the site.
