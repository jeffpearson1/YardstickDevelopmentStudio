# Intune inventory and change safety

This is how the Studio's steps treat the tenant, so you can write research that fits it and explain it to the user.

## Read-only inventory

The inventory step authenticates through the runtime's existing credential mechanism and keeps tokens and secrets out of every file. It lists the tenant's Win32 apps and searches them case-insensitively with your `searchPatterns`. A match is a **candidate, not proof of equivalence**. For each candidate it records the app ID, display name, publisher, version, created and modified times, upload state, commands, detection rules, assignments and which of your patterns matched. It changes nothing.

The check is repeated immediately before any upload, to close the race between research and publication. If a matching app has appeared, the change stops.

## Duplicate decisions

A candidate needs review when it shares a product family, executable, MSI product code or upgrade code, publisher with a close display name, or an overlapping detection footprint. Do not decide on punctuation, version suffixes or "(N-x)" naming alone.

An existing Yardstick recipe is not proof that an app exists in Intune, and an existing Intune app is not proof that it reaches the ticket's audience. Both the object and its assignments matter.

## Targets

Company Portal availability is normally an **Available** assignment to an Entra user or device group. Ticket strings may instead be asset tags, Intune device names, serials or inventory IDs; each must resolve to an exact object with a documented path to an approved group. Nothing creates a group or changes membership. When a ticket names no group, the plugin's default group is used, resolved by object ID with a fresh read and checked to be a static security group. If a display name resolves to more than one group the object ID is required. The target is never All Users or All Devices, and the intent is never `uninstall`.

## The change the user approves

Before any live work the user is shown: the tenant (ID and name, no secrets); each recipe with display name, version, source and SHA-256; the proposed `allowAvailableUninstall` value (true by default); the exact group name and object ID; the intent and notification behaviour; the test evidence; every warning, including an unsigned payload and the possibility of an incomplete object if an upload fails; and the statement that Graph DELETE is blocked. The approval is for that file's exact bytes. If it changes, the approval is void.

## After the change

Every claim is checked with fresh reads, not cached objects: upload and commit state, version, commands, detection, `allowAvailableUninstall`, assignment intent, target type and target ID. A partial upload, a mismatched app or a missing assignment is a failure for review. It is never cleaned up by deletion.
